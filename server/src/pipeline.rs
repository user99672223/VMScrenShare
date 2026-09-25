//! Capture → copy → convert → encode pipeline (two OS threads), decoupled from the WebRTC
//! session.
//!
//! * The capture thread paces itself at the configured frame rate. Every tick it reads the
//!   vkms scanout buffer (`GETPLANE` + the PRIME mapping), copies it into an ordinary buffer
//!   ([`convert::copy_xrgb`]), converts that to I420 ([`convert::xrgb_to_i420`]) and hands the
//!   frame to the encoder thread through a one-slot channel (the newest frame wins, nothing
//!   queues up). Every frame is encoded: a static desktop costs a few hundred bytes per P frame
//!   and keeps the encoder's rate control and the client's frame clock honest.
//! * The encoder thread owns the [`VideoEncoder`], applies bitrate changes, forces an IDR when
//!   a client connects, asks for one (PLI/FIR), or when `video.keyframe_interval_secs` elapsed,
//!   and pushes the encoded access units into the sink installed by the active session without
//!   ever blocking on it (a full sink drops the frame and the next one is an IDR).
//!
//! Every stage is timed ([`Stats`]); the encoder thread logs a summary every
//! [`STATS_INTERVAL`]. Both threads idle while no client is connected.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, FrameSource};
use crate::config::Config;
use crate::convert::{self, copy_xrgb, xrgb_to_i420, I420Frame, XrgbImage};
use crate::encoder::{self, EncodedFrame, EncoderSettings, VideoEncoder};

/// How often the encoder thread logs the pipeline statistics.
pub const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Accumulated timing of one pipeline stage since the last [`StageTimer::take`].
#[derive(Default)]
pub struct StageTimer {
    total_us: AtomicU64,
    max_us: AtomicU64,
    count: AtomicU64,
}

/// Snapshot returned by [`StageTimer::take`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageSummary {
    pub count: u64,
    pub avg_ms: f64,
    pub max_ms: f64,
}

impl StageTimer {
    pub fn record(&self, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        self.total_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the summary since the previous call and resets the accumulators.
    pub fn take(&self) -> StageSummary {
        let total = self.total_us.swap(0, Ordering::Relaxed);
        let max = self.max_us.swap(0, Ordering::Relaxed);
        let count = self.count.swap(0, Ordering::Relaxed);
        StageSummary {
            count,
            avg_ms: if count == 0 {
                0.0
            } else {
                total as f64 / count as f64 / 1000.0
            },
            max_ms: max as f64 / 1000.0,
        }
    }
}

/// Counters and per-stage timers shared by the capture thread, the encoder thread and the
/// session's RTP writer.
#[derive(Default)]
pub struct Stats {
    /// `GETPLANE` ioctl + copying the scanout buffer out of the PRIME mapping.
    pub capture: StageTimer,
    /// XRGB → I420 conversion.
    pub convert: StageTimer,
    /// `VideoEncoder::encode`.
    pub encode: StageTimer,
    /// Packetising and handing the access unit to the WebRTC track (`write_sample`).
    pub send: StageTimer,
    pub frames_captured: AtomicU64,
    pub frames_encoded: AtomicU64,
    pub keyframes: AtomicU64,
    pub bytes_encoded: AtomicU64,
    /// Frames dropped because the encoder was still busy with the previous one.
    pub dropped_encoder_busy: AtomicU64,
    /// Encoded frames dropped because the session's sink was full.
    pub dropped_sink_full: AtomicU64,
    /// Frames the session writer actually handed to the track.
    pub frames_sent: AtomicU64,
}

/// State shared between the pipeline threads and the session layer.
pub struct Shared {
    /// A client is connected: capture and encode.
    pub client_connected: AtomicBool,
    /// Encode the next frame as an IDR (set on connect, on PLI/FIR and after a sink drop).
    pub force_keyframe: AtomicBool,
    /// Target bitrate in kbit/s (session changes it on `SetBitrate`).
    pub bitrate_kbps: AtomicU32,
    /// Id of the session that currently owns the pipeline output.
    pub active_session: AtomicU64,
    /// Where encoded frames go (installed by the active session).
    pub sink: Mutex<Option<tokio::sync::mpsc::Sender<EncodedFrame>>>,
    pub stats: Stats,
}

impl Shared {
    pub fn new(bitrate_kbps: u32) -> Self {
        Self {
            client_connected: AtomicBool::new(false),
            force_keyframe: AtomicBool::new(false),
            bitrate_kbps: AtomicU32::new(bitrate_kbps),
            active_session: AtomicU64::new(0),
            sink: Mutex::new(None),
            stats: Stats::default(),
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
    let mut scratch = XrgbImage::default();
    let mut current = I420Frame::new(0, 0);
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
            tracing::info!(
                "client connected, capture running at {fps} fps ({} conversion)",
                convert::kernel_name()
            );
            was_connected = true;
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

        // Stage 1: GETPLANE + copy the scanout buffer out of the mapping.
        let t0 = Instant::now();
        match src.capture() {
            Ok(frame) => {
                copy_xrgb(
                    frame.data,
                    frame.width,
                    frame.height,
                    frame.pitch,
                    &mut scratch,
                );
            }
            Err(e) => {
                tracing::warn!("capture failed: {e:#}; re-locating display");
                source = None;
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        }
        let t1 = Instant::now();
        shared.stats.capture.record(t1 - t0);

        // Stage 2: XRGB -> I420.
        xrgb_to_i420(
            &scratch.data,
            scratch.width,
            scratch.height,
            scratch.pitch(),
            &mut current,
        );
        shared.stats.convert.record(t1.elapsed());
        shared.stats.frames_captured.fetch_add(1, Ordering::Relaxed);

        let keyframe = shared.force_keyframe.load(Ordering::Acquire);
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
            Ok(()) => {}
            Err(TrySendError::Full(c)) => {
                // Encoder is busy: drop this frame, recycle the buffer.
                current = c.frame;
                shared
                    .stats
                    .dropped_encoder_busy
                    .fetch_add(1, Ordering::Relaxed);
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
    let idr_interval = match config.video.keyframe_interval_secs {
        0 => None,
        s => Some(Duration::from_secs(u64::from(s))),
    };
    let mut last_idr: Option<Instant> = None;
    let mut last_stats = Instant::now();

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
        if let Some(interval) = idr_interval {
            if last_idr.is_none_or(|t| t.elapsed() >= interval) {
                force = true;
            }
        }

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

        // Stage 3: encode.
        let t0 = Instant::now();
        let encoded = enc.encode(&frame, pts_ms, force);
        shared.stats.encode.record(t0.elapsed());
        match encoded {
            Ok(Some(out)) => {
                shared.stats.frames_encoded.fetch_add(1, Ordering::Relaxed);
                shared
                    .stats
                    .bytes_encoded
                    .fetch_add(out.data.len() as u64, Ordering::Relaxed);
                if out.keyframe {
                    shared.stats.keyframes.fetch_add(1, Ordering::Relaxed);
                    last_idr = Some(Instant::now());
                } else if force {
                    // The encoder ignored the request; try again with the next frame.
                    shared.force_keyframe.store(true, Ordering::Release);
                }
                match shared.sink() {
                    Some(sink) => match sink.try_send(out) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            // The session writer is behind: drop this access unit and make
                            // sure the next one is decodable on its own.
                            shared
                                .stats
                                .dropped_sink_full
                                .fetch_add(1, Ordering::Relaxed);
                            shared.force_keyframe.store(true, Ordering::Release);
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!("frame sink closed");
                            shared.force_keyframe.store(true, Ordering::Release);
                        }
                    },
                    None => {
                        // Nobody is listening yet; make sure the first delivered frame is an IDR.
                        shared.force_keyframe.store(true, Ordering::Release);
                    }
                }
            }
            Ok(None) => {
                if force {
                    shared.force_keyframe.store(true, Ordering::Release);
                }
            }
            Err(e) => {
                tracing::error!("encode failed: {e:#}; recreating encoder");
                encoder = None;
            }
        }
        let _ = spare_tx.try_send(frame);

        if last_stats.elapsed() >= STATS_INTERVAL {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();
            log_stats(&shared.stats, secs, current_bitrate);
        }
    }
    tracing::info!("capture thread gone, encoder stopping");
}

/// Logs and resets the pipeline statistics accumulated over `secs` seconds.
pub fn log_stats(stats: &Stats, secs: f64, target_kbps: u32) {
    let captured = stats.frames_captured.swap(0, Ordering::Relaxed);
    let encoded = stats.frames_encoded.swap(0, Ordering::Relaxed);
    let sent = stats.frames_sent.swap(0, Ordering::Relaxed);
    let keyframes = stats.keyframes.swap(0, Ordering::Relaxed);
    let bytes = stats.bytes_encoded.swap(0, Ordering::Relaxed);
    let busy = stats.dropped_encoder_busy.swap(0, Ordering::Relaxed);
    let sink_full = stats.dropped_sink_full.swap(0, Ordering::Relaxed);
    let capture = stats.capture.take();
    let convert = stats.convert.take();
    let encode = stats.encode.take();
    let send = stats.send.take();
    let secs = secs.max(1e-3);
    tracing::info!(
        "pipeline: captured {:.1} fps, encoded {:.1} fps, sent {:.1} fps, {:.0} kbit/s (target {}), {} IDR, dropped {} (encoder busy) + {} (sink full)",
        captured as f64 / secs,
        encoded as f64 / secs,
        sent as f64 / secs,
        bytes as f64 * 8.0 / 1000.0 / secs,
        target_kbps,
        keyframes,
        busy,
        sink_full
    );
    tracing::info!(
        "pipeline timings avg/max ms: capture+copy {:.1}/{:.1}, convert {:.1}/{:.1}, encode {:.1}/{:.1}, packetize+send {:.1}/{:.1}",
        capture.avg_ms,
        capture.max_ms,
        convert.avg_ms,
        convert.max_ms,
        encode.avg_ms,
        encode.max_ms,
        send.avg_ms,
        send.max_ms
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_timer_accumulates_and_resets() {
        let t = StageTimer::default();
        assert_eq!(
            t.take(),
            StageSummary {
                count: 0,
                avg_ms: 0.0,
                max_ms: 0.0
            }
        );
        t.record(Duration::from_millis(2));
        t.record(Duration::from_millis(4));
        t.record(Duration::from_micros(500));
        let s = t.take();
        assert_eq!(s.count, 3);
        assert!((s.avg_ms - 6.5 / 3.0).abs() < 1e-6, "{s:?}");
        assert!((s.max_ms - 4.0).abs() < 1e-6);
        assert_eq!(t.take().count, 0);
    }

    #[test]
    fn log_stats_resets_counters() {
        let stats = Stats::default();
        stats.frames_captured.store(30, Ordering::Relaxed);
        stats.frames_encoded.store(29, Ordering::Relaxed);
        stats.dropped_sink_full.store(1, Ordering::Relaxed);
        stats.encode.record(Duration::from_millis(9));
        log_stats(&stats, 1.0, 12_000);
        assert_eq!(stats.frames_captured.load(Ordering::Relaxed), 0);
        assert_eq!(stats.frames_encoded.load(Ordering::Relaxed), 0);
        assert_eq!(stats.dropped_sink_full.load(Ordering::Relaxed), 0);
        assert_eq!(stats.encode.take().count, 0);
    }
}
