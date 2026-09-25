//! Capture → convert → encode pipeline (two OS threads), decoupled from the WebRTC session.
//!
//! * The capture thread paces itself at the configured frame rate, reads the vkms scanout
//!   buffer, converts it to I420 and hands the frame to the encoder thread through a one-slot
//!   channel (the newest frame wins, nothing queues up). Unchanged frames are skipped, except
//!   once a second and whenever a keyframe was requested.
//! * The encoder thread owns the [`VideoEncoder`], applies bitrate/keyframe requests and pushes
//!   the encoded access units into the sink installed by the active session.
//!
//! Both threads idle while no client is connected.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, FrameSource};
use crate::config::Config;
use crate::convert::{xrgb_to_i420, I420Frame};
use crate::encoder::{self, EncodedFrame, EncoderSettings, VideoEncoder};

/// State shared between the pipeline threads and the session layer.
pub struct Shared {
    /// A client is connected: capture and encode.
    pub client_connected: AtomicBool,
    /// Encode the next frame as an IDR (set on connect and on PLI/FIR).
    pub force_keyframe: AtomicBool,
    /// Target bitrate in kbit/s (session changes it on `SetBitrate`).
    pub bitrate_kbps: AtomicU32,
    /// Id of the session that currently owns the pipeline output.
    pub active_session: AtomicU64,
    /// Where encoded frames go (installed by the active session).
    pub sink: Mutex<Option<tokio::sync::mpsc::Sender<EncodedFrame>>>,
    pub frames_encoded: AtomicU64,
    pub bytes_encoded: AtomicU64,
}

impl Shared {
    pub fn new(bitrate_kbps: u32) -> Self {
        Self {
            client_connected: AtomicBool::new(false),
            force_keyframe: AtomicBool::new(false),
            bitrate_kbps: AtomicU32::new(bitrate_kbps),
            active_session: AtomicU64::new(0),
            sink: Mutex::new(None),
            frames_encoded: AtomicU64::new(0),
            bytes_encoded: AtomicU64::new(0),
        }
    }

    pub fn install_sink(&self, sink: tokio::sync::mpsc::Sender<EncodedFrame>) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    pub fn clear_sink(&self) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn sink(&self) -> Option<tokio::sync::mpsc::Sender<EncodedFrame>> {
        self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

struct Captured {
    frame: I420Frame,
    pts_ms: u64,
    /// True when the capture thread saw the keyframe flag (avoids a race where the encoder
    /// thread clears it before this frame arrives).
    keyframe: bool,
}

/// Starts the capture and encoder threads. Returns immediately; the capture thread waits for
/// the display to become active on its own.
pub fn spawn(config: Arc<Config>, shared: Arc<Shared>) -> Result<()> {
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Captured>(1);
    let (spare_tx, spare_rx) = mpsc::sync_channel::<I420Frame>(3);

    let cfg = config.clone();
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("vmdesk-capture".into())
        .spawn(move || capture_loop(cfg, sh, frame_tx, spare_rx))?;

    std::thread::Builder::new()
        .name("vmdesk-encode".into())
        .spawn(move || encode_loop(config, shared, frame_rx, spare_tx))?;
    Ok(())
}

fn capture_loop(
    config: Arc<Config>,
    shared: Arc<Shared>,
    frame_tx: mpsc::SyncSender<Captured>,
    spare_rx: mpsc::Receiver<I420Frame>,
) {
    let fps = config.video.fps.max(1);
    let frame_duration = Duration::from_secs_f64(1.0 / f64::from(fps));
    let epoch = Instant::now();
    let mut source: Option<FrameSource> = None;
    let mut current = I420Frame::new(0, 0);
    let mut last_hash: Option<u64> = None;
    let mut last_sent = Instant::now();
    let mut next_tick = Instant::now();
    let mut was_connected = false;

    loop {
        if !shared.client_connected.load(Ordering::Relaxed) {
            if was_connected {
                tracing::info!("client gone, capture idle");
                was_connected = false;
            }
            std::thread::sleep(Duration::from_millis(100));
            next_tick = Instant::now();
            continue;
        }
        if !was_connected {
            tracing::info!("client connected, capture running at {fps} fps");
            was_connected = true;
            last_hash = None;
        }

        // Pace to the frame rate; if we fell behind by more than a frame, resynchronise.
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += frame_duration;
        if next_tick + frame_duration < Instant::now() {
            next_tick = Instant::now();
        }

        let src = match source.as_mut() {
            Some(s) => s,
            None => {
                match capture::wait_for_display(
                    &config.capture.card,
                    &config.capture.connector,
                    None,
                ) {
                    Ok(s) => source.insert(s),
                    Err(e) => {
                        tracing::error!("cannot open display: {e:#}");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }
        };

        let hash = match src.capture() {
            Ok(frame) => {
                xrgb_to_i420(
                    frame.data,
                    frame.width,
                    frame.height,
                    frame.pitch,
                    &mut current,
                );
                let mut h = xxhash_rust::xxh3::Xxh3::new();
                h.update(&current.y);
                h.update(&current.u);
                h.update(&current.v);
                h.digest()
            }
            Err(e) => {
                tracing::warn!("capture failed: {e:#}; re-locating display");
                source = None;
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };

        let keyframe = shared.force_keyframe.load(Ordering::Acquire);
        let unchanged = last_hash == Some(hash);
        if unchanged && !keyframe && last_sent.elapsed() < Duration::from_secs(1) {
            continue;
        }

        let spare = spare_rx
            .try_recv()
            .unwrap_or_else(|_| I420Frame::new(current.width, current.height));
        let frame = std::mem::replace(&mut current, spare);
        let captured = Captured {
            frame,
            pts_ms: epoch.elapsed().as_millis() as u64,
            keyframe,
        };
        match frame_tx.try_send(captured) {
            Ok(()) => {
                last_hash = Some(hash);
                last_sent = Instant::now();
            }
            Err(TrySendError::Full(c)) => {
                // Encoder is busy: drop this frame, recycle the buffer.
                current = c.frame;
                tracing::trace!("encoder busy, frame dropped");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("encoder thread gone, stopping capture");
                return;
            }
        }
    }
}

fn encode_loop(
    config: Arc<Config>,
    shared: Arc<Shared>,
    frame_rx: mpsc::Receiver<Captured>,
    spare_tx: mpsc::SyncSender<I420Frame>,
) {
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut encoder_size = (0usize, 0usize);
    let mut current_bitrate = shared.bitrate_kbps.load(Ordering::Relaxed);
    let mut last_stats = Instant::now();
    let mut stat_frames = 0u64;
    let mut stat_bytes = 0u64;

    while let Ok(captured) = frame_rx.recv() {
        let Captured {
            frame,
            pts_ms,
            keyframe,
        } = captured;
        if frame.width == 0 || frame.height == 0 {
            continue;
        }
        let mut force = keyframe | shared.force_keyframe.swap(false, Ordering::AcqRel);

        if encoder.is_none() || encoder_size != (frame.width, frame.height) {
            let settings = EncoderSettings {
                width: frame.width as u32,
                height: frame.height as u32,
                fps: config.video.fps,
                bitrate_kbps: shared.bitrate_kbps.load(Ordering::Relaxed),
                keyframe_interval: config.video.keyframe_interval,
                threads: config.video.encoder_threads,
            };
            match encoder::create(settings) {
                Ok(enc) => {
                    tracing::info!(
                        "encoder {} created for {}x{}",
                        enc.name(),
                        frame.width,
                        frame.height
                    );
                    encoder = Some(enc);
                    encoder_size = (frame.width, frame.height);
                    current_bitrate = settings.bitrate_kbps;
                    force = true;
                }
                Err(e) => {
                    tracing::error!("cannot create encoder: {e:#}");
                    std::thread::sleep(Duration::from_secs(1));
                    let _ = spare_tx.try_send(frame);
                    continue;
                }
            }
        }
        let enc = encoder.as_mut().expect("encoder present");

        let wanted = shared.bitrate_kbps.load(Ordering::Relaxed);
        if wanted != current_bitrate {
            match enc.set_bitrate(wanted) {
                Ok(()) => current_bitrate = wanted,
                Err(e) => tracing::warn!("bitrate change failed: {e:#}"),
            }
        }

        match enc.encode(&frame, pts_ms, force) {
            Ok(Some(out)) => {
                stat_frames += 1;
                stat_bytes += out.data.len() as u64;
                shared.frames_encoded.fetch_add(1, Ordering::Relaxed);
                shared
                    .bytes_encoded
                    .fetch_add(out.data.len() as u64, Ordering::Relaxed);
                if let Some(sink) = shared.sink() {
                    if sink.blocking_send(out).is_err() {
                        tracing::debug!("frame sink closed");
                    }
                } else if force {
                    // Nobody is listening yet; make sure the next frame is a keyframe again.
                    shared.force_keyframe.store(true, Ordering::Release);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("encode failed: {e:#}; recreating encoder");
                encoder = None;
            }
        }
        let _ = spare_tx.try_send(frame);

        if last_stats.elapsed() >= Duration::from_secs(10) {
            let secs = last_stats.elapsed().as_secs_f64();
            tracing::info!(
                "encoded {:.1} fps, {:.0} kbit/s (target {} kbit/s)",
                stat_frames as f64 / secs,
                stat_bytes as f64 * 8.0 / 1000.0 / secs,
                current_bitrate
            );
            last_stats = Instant::now();
            stat_frames = 0;
            stat_bytes = 0;
        }
    }
    tracing::info!("capture thread gone, encoder stopping");
}
