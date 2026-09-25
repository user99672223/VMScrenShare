//! Full WebRTC loopback between the server stack and the client stack, in one process.
//!
//! The server side is built exactly like `session::accept_offer` does it (ICE-lite, the same
//! interceptors, the same track and writer); the client side uses the client library's
//! peer-connection helpers and access unit assembler. Real OpenH264 frames flow over UDP on
//! the host's interface. Two scenarios:
//!
//! * a clean path: no `duplicated` SRTP/SRTCP replay errors on either side, no NACKs (a NACK
//!   for a packet that did arrive makes the server retransmit it, which the receiver's replay
//!   protection then rejects), every access unit reassembled and decoded;
//! * a lossy path (the server's sockets drop every N-th video datagram): the client NACKs the
//!   gaps, the retransmissions are accepted (replay window wide enough for the keyframe
//!   bursts they arrive behind), still no `duplicated` errors, and every frame decodes.

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use client::decoder::ffmpeg::FfmpegDecoder;
use client::decoder::{HwPreference, VideoDecoder};
use client::net as cnet;
use e2e::LogCapture;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use server::encoder::{self, EncodedFrame, EncoderSettings};
use server::pipeline::Shared;
use server::session as ssession;
use server::testpattern;
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCPeerConnectionState, RTCSessionDescription,
};
use webrtc::runtime::{
    AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, RecvMeta, Runtime,
    TokioRuntime, Transmit,
};

const W: usize = 1280;
const H: usize = 720;
const FPS: u32 = 30;
const FRAMES: usize = 90;
const BITRATE_KBPS: u32 = 6000;
const SSRC: u32 = 0x1234_5678;
/// Datagrams at least this large are video RTP (DTLS, STUN, SCTP and RTCP stay well below).
const VIDEO_DATAGRAM_MIN: usize = 1000;

/// The two tests share one process-wide log capture; run them one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------------------------
// A runtime whose UDP sockets drop every N-th large datagram once armed.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct LossyRuntime {
    inner: TokioRuntime,
    every: u64,
    armed: Arc<AtomicBool>,
    counter: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl LossyRuntime {
    fn new(every: u64) -> Self {
        Self {
            inner: TokioRuntime,
            every,
            armed: Arc::new(AtomicBool::new(false)),
            counter: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Debug)]
struct LossySocket {
    inner: Arc<dyn AsyncUdpSocket>,
    every: u64,
    armed: Arc<AtomicBool>,
    counter: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl AsyncUdpSocket for LossySocket {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn poll_send(
        &self,
        cx: &mut TaskContext<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<usize>> {
        if self.armed.load(Ordering::Relaxed) && transmit.contents.len() >= VIDEO_DATAGRAM_MIN {
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(self.every) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Poll::Ready(Ok(transmit.contents.len()));
            }
        }
        self.inner.poll_send(cx, transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut TaskContext<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    /// No GSO: one datagram per send, so a drop is one packet.
    fn max_gso_segments(&self) -> usize {
        1
    }

    fn max_gro_segments(&self) -> usize {
        self.inner.max_gro_segments()
    }
}

impl Runtime for LossyRuntime {
    fn spawn(
        &self,
        future: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) -> Box<dyn JoinHandle> {
        self.inner.spawn(future)
    }

    fn wrap_udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let inner = self.inner.wrap_udp_socket(socket)?;
        Ok(Arc::new(LossySocket {
            inner,
            every: self.every,
            armed: Arc::clone(&self.armed),
            counter: Arc::clone(&self.counter),
            dropped: Arc::clone(&self.dropped),
        }))
    }

    fn wrap_tcp_listener(
        &self,
        listener: std::net::TcpListener,
    ) -> io::Result<Arc<dyn AsyncTcpListener>> {
        self.inner.wrap_tcp_listener(listener)
    }

    fn connect_tcp<'a>(
        &'a self,
        remote_addr: SocketAddr,
    ) -> Pin<Box<dyn std::future::Future<Output = io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>>
    {
        self.inner.connect_tcp(remote_addr)
    }

    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        self.inner.resolve_host(host)
    }

    fn sleep(
        &self,
        duration: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        self.inner.sleep(duration)
    }

    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        self.inner.interval(period)
    }

    fn block_on(&self, future: Pin<Box<dyn std::future::Future<Output = ()> + '_>>) {
        self.inner.block_on(future)
    }

    fn yield_now(&self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        self.inner.yield_now()
    }

    fn name(&self) -> &'static str {
        "lossy-tokio"
    }
}

// ---------------------------------------------------------------------------------------------
// Shared scenario
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct ServerFeedback {
    nacks: Mutex<Vec<Vec<u16>>>,
    plis: AtomicU64,
    receiver_reports: AtomicU64,
}

struct Outcome {
    sent: u64,
    packets: u64,
    access_units: u64,
    pictures: u64,
    decode_errors: u64,
    nacks: Vec<Vec<u16>>,
    plis: u64,
    duplicated: Vec<e2e::CapturedRecord>,
    warnings: Vec<e2e::CapturedRecord>,
    assembler_dropped_units: u64,
}

fn encode_frames() -> Vec<EncodedFrame> {
    let mut enc = encoder::create(EncoderSettings {
        width: W as u32,
        height: H as u32,
        fps: FPS,
        bitrate_kbps: BITRATE_KBPS,
        keyframe_interval: 0,
        threads: 2,
    })
    .expect("encoder");
    (0..FRAMES)
        .map(|i| {
            enc.encode(
                &testpattern::i420(W, H, i),
                i as u64 * 1000 / u64::from(FPS),
                i == 0 || i == FRAMES / 2,
            )
            .expect("encode")
            .expect("no frame skipping")
        })
        .collect()
}

async fn wait_gathering(
    events: &mut mpsc::UnboundedReceiver<ssession::PcEvent>,
    pending: &mut Vec<ssession::PcEvent>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline.into(), events.recv()).await {
            Ok(Some(ssession::PcEvent::Gathering(RTCIceGatheringState::Complete))) => return Ok(()),
            Ok(Some(ssession::PcEvent::Gathering(_))) => {}
            Ok(Some(ev)) => pending.push(ev),
            Ok(None) => return Err(anyhow!("server driver stopped")),
            Err(_) => return Err(anyhow!("server gathering timed out")),
        }
    }
}

async fn read_client_track(
    track: Arc<dyn TrackRemote>,
    access_units: Arc<AtomicU64>,
    packets: Arc<AtomicU64>,
    dropped_units: Arc<AtomicU64>,
    au_tx: std::sync::mpsc::Sender<bytes::Bytes>,
) {
    let mut assembler = cnet::new_assembler();
    while let Some(ev) = track.poll().await {
        match ev {
            TrackRemoteEvent::OnRtpPacket(p) => {
                packets.fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                assembler.push(now, p);
                while let Some(au) = assembler.pop(now) {
                    access_units.fetch_add(1, Ordering::Relaxed);
                    let _ = au_tx.send(au);
                }
                dropped_units.store(assembler.stats.dropped_units, Ordering::Relaxed);
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
    eprintln!("client assembler: {:?}", assembler.stats);
}

async fn observe_server_rtcp(
    track: Arc<webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample>,
    feedback: Arc<ServerFeedback>,
) {
    while let Some(ev) = track.poll().await {
        if let TrackLocalEvent::OnRtcpPacket(packets) = ev {
            for p in packets {
                let any = p.as_any();
                if let Some(nack) = any.downcast_ref::<TransportLayerNack>() {
                    let seqs: Vec<u16> = nack.nacks.iter().flat_map(|n| n.packet_list()).collect();
                    eprintln!("server got NACK for {} packet(s): {:?}", seqs.len(), seqs);
                    feedback.nacks.lock().unwrap().push(seqs);
                } else if any.is::<PictureLossIndication>() {
                    feedback.plis.fetch_add(1, Ordering::Relaxed);
                } else if any.is::<ReceiverReport>() {
                    feedback.receiver_reports.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Runs the whole scenario. `server_runtime` replaces the server's I/O runtime (the lossy
/// one); `on_connected` runs once the server reports the connection up.
async fn run_loopback(
    port: u16,
    server_runtime: Option<Arc<dyn Runtime>>,
    on_connected: impl FnOnce() + Send + 'static,
) -> Result<Outcome> {
    let logs = LogCapture::install(true);
    logs.clear();
    let frames = tokio::task::spawn_blocking(encode_frames).await?;
    let total_bytes: usize = frames.iter().map(|f| f.data.len()).sum();
    eprintln!(
        "encoded {} frames, {} KB; keyframes: {:?}",
        frames.len(),
        total_bytes / 1024,
        frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.keyframe)
            .map(|(i, f)| format!("#{i} {} bytes {:?}", f.data.len(), e2e::nal_types(&f.data)))
            .collect::<Vec<_>>()
    );

    // ---- server peer connection (as in session::accept_offer) ----
    let use_ipv6 = server::netinfo::global_ipv6().is_some();
    let parts = ssession::peer_connection_parts(port, use_ipv6, true)?;
    let (s_events_tx, mut s_events) = mpsc::unbounded_channel();
    let mut builder = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(parts.media_engine)
        .with_setting_engine(parts.setting_engine)
        .with_interceptor_registry(parts.registry)
        .with_handler(Arc::new(ssession::Handler {
            events: s_events_tx,
        }))
        .with_udp_addrs(parts.udp_addrs);
    if let Some(rt) = server_runtime {
        builder = builder.with_runtime(rt);
    }
    let server_pc: Arc<dyn PeerConnection> =
        Arc::new(builder.build().await.context("server peer connection")?);
    let track = ssession::video_track(SSRC)?;
    let sender = server_pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await
        .context("add_track")?;

    // ---- client peer connection (as in net::run) ----
    let (c_events_tx, mut c_events) = mpsc::unbounded_channel();
    let client_pc = cnet::create_peer_connection(Arc::new(cnet::Handler {
        events: c_events_tx,
    }))
    .await?;
    let channels = cnet::add_channels_and_video(&client_pc).await?;
    let offer = client_pc.create_offer(None).await?;
    client_pc.set_local_description(offer).await?;
    let mut c_pending = Vec::new();
    cnet::wait_for_gathering(&mut c_events, &mut c_pending).await?;
    let offer_sdp = client_pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no client local description"))?
        .sdp;

    // ---- signalling, in process ----
    server_pc
        .set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
        .await
        .context("server set_remote_description")?;
    let answer = server_pc.create_answer(None).await?;
    server_pc.set_local_description(answer).await?;
    let mut s_pending = Vec::new();
    wait_gathering(&mut s_events, &mut s_pending).await?;
    let answer_sdp = server_pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no server local description"))?
        .sdp;
    assert!(proto::sdp::is_ice_lite(&answer_sdp));
    client_pc
        .set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await
        .context("client set_remote_description")?;

    let deadline = Instant::now() + Duration::from_secs(15);

    // ---- server side: once connected, stream like the session writer does ----
    // Runs concurrently with the client side below: the client only learns about the track
    // from the first RTP packet, exactly as in the real deployment.
    let shared = Arc::new(Shared::new(BITRATE_KBPS));
    let feedback = Arc::new(ServerFeedback::default());
    tokio::spawn(observe_server_rtcp(
        Arc::clone(&track),
        Arc::clone(&feedback),
    ));
    let server_task = {
        let track = Arc::clone(&track);
        let shared = Arc::clone(&shared);
        let server_pc = Arc::clone(&server_pc);
        tokio::spawn(async move {
            let mut connected = s_pending.iter().any(|e| {
                matches!(
                    e,
                    ssession::PcEvent::Connection(RTCPeerConnectionState::Connected)
                )
            });
            while !connected {
                match tokio::time::timeout_at(deadline.into(), s_events.recv())
                    .await
                    .context("server never connected")?
                {
                    Some(ssession::PcEvent::Connection(RTCPeerConnectionState::Connected)) => {
                        connected = true
                    }
                    Some(ssession::PcEvent::Connection(
                        s @ (RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed),
                    )) => return Err(anyhow!("server connection {s}")),
                    Some(_) => {}
                    None => return Err(anyhow!("server driver stopped")),
                }
            }
            eprintln!(
                "server connected: {:?}",
                ssession::selected_path(&server_pc).await
            );
            on_connected();
            let (frame_tx, frame_rx) = mpsc::channel::<EncodedFrame>(8);
            let writer = tokio::spawn(ssession::write_frames(
                track,
                sender,
                frame_rx,
                Duration::from_millis(1000 / u64::from(FPS)),
                shared,
            ));
            let mut tick = tokio::time::interval(Duration::from_millis(1000 / u64::from(FPS)));
            for frame in &frames {
                tick.tick().await;
                frame_tx.send(frame.clone()).await.context("writer gone")?;
            }
            // Let the last packets, retransmissions and feedback settle.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            drop(frame_tx);
            let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
            Ok::<(), anyhow::Error>(())
        })
    };

    // ---- client side: wait for the track, reassemble + decode ----
    let access_units = Arc::new(AtomicU64::new(0));
    let packets = Arc::new(AtomicU64::new(0));
    let dropped_units = Arc::new(AtomicU64::new(0));
    let (au_tx, au_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
    let decoder_thread = std::thread::spawn(move || {
        let mut decoder = FfmpegDecoder::new(HwPreference::Software).expect("software decoder");
        let mut pictures = 0u64;
        let mut errors = 0u64;
        let mut out = Vec::new();
        let mut index = 0usize;
        let mut reported = 0;
        // An empty access unit is the end-of-test sentinel (the reader task keeps its sender
        // alive while it waits on the track).
        while let Ok(au) = au_rx.recv() {
            if au.is_empty() {
                break;
            }
            out.clear();
            let result = decoder.decode(&au, &mut out);
            if index < 2 || (out.len() != 1 && reported < 10) {
                eprintln!(
                    "client AU #{index}: {} bytes, NAL types {:?}, pictures {}, result {:?}",
                    au.len(),
                    e2e::nal_types(&au),
                    out.len(),
                    result.as_ref().map(|_| ())
                );
                if out.len() != 1 {
                    reported += 1;
                }
            }
            match result {
                Ok(()) => pictures += out.len() as u64,
                Err(_) => errors += 1,
            }
            index += 1;
        }
        (pictures, errors)
    });

    let mut client_connected = false;
    let mut client_track: Option<(Arc<dyn TrackRemote>, u32)> = None;
    let mut pli = cnet::PliLimiter::new(cnet::PLI_INTERVAL);
    let mut queued = std::mem::take(&mut c_pending).into_iter();
    while !(client_connected && client_track.is_some()) {
        let ev = match queued.next() {
            Some(ev) => ev,
            None => tokio::time::timeout_at(deadline.into(), c_events.recv())
                .await
                .context("client never connected / no track")?
                .ok_or_else(|| anyhow!("client driver stopped"))?,
        };
        match ev {
            cnet::PcEvent::Connection(RTCPeerConnectionState::Connected) => {
                client_connected = true;
                eprintln!("client connected");
            }
            cnet::PcEvent::Connection(
                s @ (RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed),
            ) => {
                return Err(anyhow!("client connection {s}"));
            }
            cnet::PcEvent::Track(track) => {
                let ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
                eprintln!("client got track ssrc {ssrc}");
                tokio::spawn(read_client_track(
                    Arc::clone(&track),
                    Arc::clone(&access_units),
                    Arc::clone(&packets),
                    Arc::clone(&dropped_units),
                    au_tx.clone(),
                ));
                // The real client asks for a keyframe as soon as the track appears.
                pli.request(&track, ssrc).await;
                client_track = Some((track, ssrc));
            }
            _ => {}
        }
    }
    server_task.await??;
    eprintln!("streaming finished");

    // Close both sides so the decoder thread finishes.
    let _ = tokio::time::timeout(Duration::from_secs(3), channels.control.close()).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), client_pc.close()).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), server_pc.close()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(client_track);
    let _ = au_tx.send(bytes::Bytes::new());
    let (pictures, decode_errors) =
        tokio::task::spawn_blocking(move || decoder_thread.join().expect("decoder thread")).await?;

    let nacks = feedback.nacks.lock().unwrap().clone();
    let outcome = Outcome {
        sent: shared.stats.frames_sent.load(Ordering::Relaxed),
        packets: packets.load(Ordering::Relaxed),
        access_units: access_units.load(Ordering::Relaxed),
        pictures,
        decode_errors,
        nacks,
        plis: feedback.plis.load(Ordering::Relaxed),
        duplicated: logs.warnings_containing("duplicated"),
        warnings: logs
            .records()
            .into_iter()
            .filter(|r| r.level <= log::Level::Warn)
            .collect(),
        assembler_dropped_units: dropped_units.load(Ordering::Relaxed),
    };
    let nacked_packets: usize = outcome.nacks.iter().map(Vec::len).sum();
    eprintln!(
        "sent {} frames, client got {} packets / {} access units, decoded {} pictures ({} errors); \
         NACKs {} ({nacked_packets} packets), PLIs {}, RRs {}, 'duplicated' warnings {}, warnings total {}",
        outcome.sent,
        outcome.packets,
        outcome.access_units,
        outcome.pictures,
        outcome.decode_errors,
        outcome.nacks.len(),
        outcome.plis,
        feedback.receiver_reports.load(Ordering::Relaxed),
        outcome.duplicated.len(),
        outcome.warnings.len()
    );
    for w in outcome.warnings.iter().take(20) {
        eprintln!("  warning: [{}] {}", w.target, w.message);
    }
    Ok(outcome)
}

fn assert_common(o: &Outcome) {
    assert_eq!(o.sent, FRAMES as u64, "the writer did not send every frame");
    assert!(
        o.duplicated.is_empty(),
        "SRTP replay errors: {} (first: {:?})",
        o.duplicated.len(),
        o.duplicated.first().map(|r| &r.message)
    );
    assert!(
        o.plis >= 1,
        "the client's initial PLI never reached the server"
    );
    assert_eq!(
        o.access_units, o.sent,
        "client reassembled {} of {} access units",
        o.access_units, o.sent
    );
    assert_eq!(
        (o.pictures, o.decode_errors),
        (o.sent, 0),
        "decoded {} pictures from {} access units with {} errors",
        o.pictures,
        o.access_units,
        o.decode_errors
    );
    assert_eq!(o.assembler_dropped_units, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_delivers_every_frame_exactly_once() -> Result<()> {
    let _guard = SERIAL.lock().await;
    let o = run_loopback(50_777, None, || {}).await?;
    assert_common(&o);
    let nacked: usize = o.nacks.iter().map(Vec::len).sum();
    assert!(
        o.nacks.is_empty(),
        "client NACKed {nacked} packets on a loss-free path: {:?}",
        o.nacks
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_recovers_lost_packets_with_nack() -> Result<()> {
    let _guard = SERIAL.lock().await;
    let lossy = Arc::new(LossyRuntime::new(23));
    let armed = Arc::clone(&lossy.armed);
    let dropped = Arc::clone(&lossy.dropped);
    let o = run_loopback(50_778, Some(lossy as Arc<dyn Runtime>), move || {
        armed.store(true, Ordering::Relaxed)
    })
    .await?;
    let dropped = dropped.load(Ordering::Relaxed);
    let nacked: usize = o.nacks.iter().map(Vec::len).sum();
    eprintln!("lossy socket dropped {dropped} datagrams, client NACKed {nacked} packets");
    assert!(
        dropped >= 5,
        "the lossy socket dropped only {dropped} datagrams"
    );
    assert!(
        nacked as u64 >= dropped,
        "client NACKed {nacked} packets for {dropped} dropped datagrams"
    );
    assert_common(&o);
    Ok(())
}
