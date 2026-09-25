//! WebRTC session: one peer connection per client (a new offer replaces the previous client).
//!
//! * ICE-lite, host candidates only, one UDP port from the configured range per connection,
//!   bound on IPv4 and (when the VM has a global address) IPv6. The IPv4 sits behind OCI's 1:1
//!   NAT, so IPv4 host candidates are rewritten to the public address before the answer is
//!   returned (see [`proto::sdp::rewrite_host_candidates`]); IPv6 candidates carry the
//!   interface's global address unless `network.public_ipv6` overrides it. The client's ICE
//!   agent then picks whichever path works.
//! * One H.264 video track fed by the encoder pipeline through [`Shared::sink`].
//! * Two data channels created by the client: `control` (reliable) and `mouse` (unreliable).
//! * PLI/FIR from the client force an IDR through [`Shared::force_keyframe`].

use std::net::Ipv6Addr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::ice::network_type::NetworkType;
use rtc::interceptor::Slot;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidate, RTCIceConnectionState,
    RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription, Registry,
    SettingEngineBuilder,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::config::Config;
use crate::input::{InputEvent, InputSink};
use crate::metadata;
use crate::pipeline::Shared;
use crate::rtcp_forward::{is_keyframe_request, KeyframeRequestForwarder};

const MIME_TYPE_H264: &str = "video/H264";
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const H264_PAYLOAD_TYPE: u8 = 102;
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The offer is unusable (client bug): HTTP 400.
    #[error("bad offer: {0}")]
    BadOffer(String),
    /// Something failed on our side: HTTP 500.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type SharedInput = Arc<Mutex<Box<dyn InputSink>>>;

/// Addressing facts gathered at startup.
#[derive(Debug, Clone)]
pub struct NetInfo {
    /// Public IPv4 from the config or the metadata service (may be filled in later).
    pub public_ipv4: Option<String>,
    /// `network.public_ipv6` override, if any.
    pub public_ipv6: Option<String>,
    /// Global IPv6 address found on an interface (None = the VM has no IPv6).
    pub local_ipv6: Option<Ipv6Addr>,
    /// `network.ipv6`.
    pub ipv6_enabled: bool,
}

impl NetInfo {
    /// Whether to bind IPv6 sockets and advertise IPv6 candidates.
    pub fn use_ipv6(&self) -> bool {
        self.ipv6_enabled && (self.local_ipv6.is_some() || self.public_ipv6.is_some())
    }
}

pub struct SessionManager {
    config: Arc<Config>,
    shared: Arc<Shared>,
    input: SharedInput,
    net: NetInfo,
    public_ipv4: Mutex<Option<String>>,
    next_port: Mutex<u16>,
    next_session: Mutex<u64>,
    current: tokio::sync::Mutex<Option<Arc<dyn PeerConnection>>>,
}

enum PcEvent {
    Connection(RTCPeerConnectionState),
    Gathering(RTCIceGatheringState),
    DataChannel(Arc<dyn DataChannel>),
}

struct Handler {
    events: mpsc::UnboundedSender<PcEvent>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        let _ = self.events.send(PcEvent::Gathering(state));
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        tracing::debug!("ICE connection state: {state}");
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.events.send(PcEvent::Connection(state));
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.events.send(PcEvent::DataChannel(data_channel));
    }
}

impl SessionManager {
    pub fn new(config: Arc<Config>, shared: Arc<Shared>, input: SharedInput, net: NetInfo) -> Self {
        let first_port = config.network.udp_port_min;
        let public_ipv4 = net.public_ipv4.clone();
        Self {
            config,
            shared,
            input,
            net,
            public_ipv4: Mutex::new(public_ipv4),
            next_port: Mutex::new(first_port),
            next_session: Mutex::new(1),
            current: tokio::sync::Mutex::new(None),
        }
    }

    fn allocate_port(&self) -> u16 {
        let (min, max) = (
            self.config.network.udp_port_min,
            self.config.network.udp_port_max,
        );
        let mut next = self.next_port.lock().unwrap_or_else(|e| e.into_inner());
        let port = (*next).clamp(min, max);
        *next = if port >= max { min } else { port + 1 };
        port
    }

    fn allocate_session_id(&self) -> u64 {
        let mut n = self.next_session.lock().unwrap_or_else(|e| e.into_inner());
        let id = *n;
        *n += 1;
        id
    }

    /// Public IPv4 from config, cache or (retried) metadata lookup.
    async fn public_ipv4(&self) -> Option<String> {
        if !self.config.network.public_ip.is_empty() {
            return Some(self.config.network.public_ip.clone());
        }
        if let Some(ip) = self
            .public_ipv4
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(ip);
        }
        match metadata::detect_public_ip(&self.config.network.metadata_url).await {
            Ok(ip) => {
                tracing::info!("public IPv4 {ip} (OCI metadata)");
                *self.public_ipv4.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip.clone());
                Some(ip)
            }
            Err(e) => {
                tracing::warn!(
                    "public IPv4 unknown ({e:#}); IPv4 candidates will carry the VM's private address. \
                     Fix: sudo ./server setup --skip-apt --public-ip <ip>"
                );
                None
            }
        }
    }

    /// Handles `POST /offer`: tears down the previous client, negotiates a new peer connection
    /// and returns the complete answer SDP.
    pub async fn accept_offer(self: &Arc<Self>, offer_sdp: String) -> Result<String, SessionError> {
        if !proto::sdp::has_media(&offer_sdp, "video") {
            return Err(SessionError::BadOffer(
                "offer has no video media section".into(),
            ));
        }
        if !proto::sdp::has_media(&offer_sdp, "application") {
            return Err(SessionError::BadOffer(
                "offer has no data channel (application) media section".into(),
            ));
        }

        // Only one client at a time: replace the previous session. Take the connection out of
        // the mutex first so the guard is not held across `close().await`.
        let previous = self.current.lock().await.take();
        if let Some(old) = previous {
            tracing::info!("new offer received, closing the previous session");
            self.shared.client_connected.store(false, Ordering::Release);
            self.shared.clear_sink();
            release_input(&self.input);
            let _ = old.close().await;
        }

        let session_id = self.allocate_session_id();
        let port = self.allocate_port();
        let public_ipv4 = self.public_ipv4().await;
        let use_ipv6 = self.net.use_ipv6();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let codec = h264_codec();
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(codec.clone(), RtpCodecKind::Video)
            .context("registering H264")?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("default interceptors")?
            .with(
                Slot::from(crate::rtcp_forward::SLOT),
                KeyframeRequestForwarder::new(),
            );
        let mut network_types = vec![NetworkType::Udp4];
        let mut udp_addrs = vec![format!("0.0.0.0:{port}")];
        if use_ipv6 {
            network_types.push(NetworkType::Udp6);
            udp_addrs.push(format!("[::]:{port}"));
        }
        let setting_engine = SettingEngineBuilder::new()
            .with_lite(true)
            .with_network_types(network_types)
            .with_multicast_dns_mode(MulticastDnsMode::Disabled)
            .build();

        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media_engine)
            .with_setting_engine(setting_engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { events: events_tx }))
            .with_udp_addrs(udp_addrs)
            .build()
            .await
            .with_context(|| format!("creating peer connection on UDP port {port}"))?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let ssrc = pseudo_random_ssrc(session_id);
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "vmdesk".to_string(),
                    "vmdesk-video".to_string(),
                    "vmdesk video".to_string(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec: codec.rtp_codec.clone(),
                        ..Default::default()
                    }],
                ),
            )
            .context("creating video track")?,
        );
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .context("adding video track")?;

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| SessionError::BadOffer(format!("unparseable SDP: {e}")))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| SessionError::BadOffer(format!("set_remote_description: {e}")))?;
        let answer = pc.create_answer(None).await.context("create_answer")?;
        pc.set_local_description(answer)
            .await
            .context("set_local_description")?;

        // Non-trickle: wait until every candidate is in the local description. Other events
        // that arrive meanwhile are kept for the session task.
        let mut events_rx = events_rx;
        let mut pending: Vec<PcEvent> = Vec::new();
        let deadline = Instant::now() + GATHER_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline.into(), events_rx.recv()).await {
                Ok(Some(PcEvent::Gathering(RTCIceGatheringState::Complete))) => break,
                Ok(Some(PcEvent::Gathering(_))) => {}
                Ok(Some(ev)) => pending.push(ev),
                Ok(None) => {
                    return Err(anyhow!("peer connection driver stopped during gathering").into())
                }
                Err(_) => {
                    tracing::warn!("ICE gathering did not complete within {GATHER_TIMEOUT:?}");
                    break;
                }
            }
        }

        let local = pc
            .local_description()
            .await
            .ok_or_else(|| anyhow!("no local description after create_answer"))?;
        let mut sdp = local.sdp;
        if proto::sdp::candidate_count(&sdp) == 0 {
            let _ = pc.close().await;
            return Err(anyhow!(
                "no ICE candidates gathered on UDP port {port}; is the port free and does the VM have a non-loopback interface?"
            )
            .into());
        }
        sdp = proto::sdp::rewrite_host_candidates(
            &sdp,
            public_ipv4.as_deref(),
            self.net.public_ipv6.as_deref(),
        );
        let advertised = proto::sdp::host_candidate_addresses(&sdp);
        tracing::info!(
            "session {session_id}: answer ready, UDP port {port}, host candidates {} ({}), ice-lite {}",
            advertised.join(", "),
            advertised
                .iter()
                .map(|a| proto::sdp::address_family(a))
                .collect::<Vec<_>>()
                .join("/"),
            proto::sdp::is_ice_lite(&sdp)
        );

        *self.current.lock().await = Some(Arc::clone(&pc));
        self.shared
            .active_session
            .store(session_id, Ordering::Release);

        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            // Replay events that arrived during gathering, then run the session.
            let (replay_tx, replay_rx) = mpsc::unbounded_channel();
            for ev in pending.drain(..) {
                let _ = replay_tx.send(ev);
            }
            let events = merge_events(replay_rx, events_rx);
            run_session(mgr, session_id, pc, track, sender, events).await;
        });

        Ok(sdp)
    }
}

/// Chains two receivers into one.
fn merge_events(
    mut first: mpsc::UnboundedReceiver<PcEvent>,
    mut second: mpsc::UnboundedReceiver<PcEvent>,
) -> mpsc::UnboundedReceiver<PcEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(ev) = first.try_recv() {
            if tx.send(ev).is_err() {
                return;
            }
        }
        while let Some(ev) = second.recv().await {
            if tx.send(ev).is_err() {
                return;
            }
        }
    });
    rx
}

fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.to_owned(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: String::new(),
                },
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "pli".to_owned(),
                },
                RTCPFeedback {
                    typ: "ccm".to_owned(),
                    parameter: "fir".to_owned(),
                },
            ],
        },
        payload_type: H264_PAYLOAD_TYPE,
    }
}

fn pseudo_random_ssrc(session_id: u64) -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = nanos ^ session_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    ((x as u32) | 1).max(2)
}

fn release_input(input: &SharedInput) {
    if let Err(e) = input
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .release_all()
    {
        tracing::warn!("release_all failed: {e:#}");
    }
}

fn endpoint(c: &RTCIceCandidate) -> String {
    proto::sdp::format_endpoint(&c.address, c.port)
}

/// `"IPv6: local [..]:p <-> remote [..]:p"` for the nominated candidate pair, if known yet.
pub async fn selected_path(pc: &Arc<dyn PeerConnection>) -> Option<String> {
    let sctp = pc.sctp().await?;
    let ice = sctp.transport().ice_transport();
    let pair = ice.get_selected_candidate_pair().await.ok().flatten()?;
    let remote = pair.remote();
    Some(format!(
        "{}: local {} <-> remote {}",
        proto::sdp::address_family(&remote.address),
        endpoint(pair.local()),
        endpoint(remote)
    ))
}

async fn run_session(
    mgr: Arc<SessionManager>,
    session_id: u64,
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    mut events: mpsc::UnboundedReceiver<PcEvent>,
) {
    let shared = Arc::clone(&mgr.shared);
    let (frame_tx, frame_rx) = mpsc::channel::<crate::encoder::EncodedFrame>(8);
    let frame_duration = Duration::from_micros(1_000_000 / u64::from(mgr.config.video.fps.max(1)));

    // Encoded frames → RTP.
    let writer_track = Arc::clone(&track);
    let writer = tokio::spawn(async move {
        write_frames(writer_track, sender, frame_rx, frame_duration).await;
    });

    // Keyframe requests (PLI/FIR) from the client.
    let rtcp_track = Arc::clone(&track);
    let rtcp_shared = Arc::clone(&shared);
    let rtcp = tokio::spawn(async move {
        while let Some(ev) = rtcp_track.poll().await {
            #[allow(clippy::single_match)]
            match ev {
                TrackLocalEvent::OnRtcpPacket(packets) => {
                    if packets.iter().any(|p| is_keyframe_request(p.as_ref())) {
                        tracing::debug!("keyframe requested by client");
                        rtcp_shared.force_keyframe.store(true, Ordering::Release);
                    }
                }
                _ => {}
            }
        }
    });

    let mut connected = false;
    while let Some(ev) = events.recv().await {
        match ev {
            PcEvent::Connection(state) => {
                tracing::info!("session {session_id}: connection state {state}");
                match state {
                    RTCPeerConnectionState::Connected => {
                        if shared.active_session.load(Ordering::Acquire) != session_id {
                            break;
                        }
                        shared
                            .bitrate_kbps
                            .store(mgr.config.video.bitrate_kbps, Ordering::Relaxed);
                        shared.install_sink(frame_tx.clone());
                        shared.force_keyframe.store(true, Ordering::Release);
                        shared.client_connected.store(true, Ordering::Release);
                        connected = true;
                        let pc_for_log = Arc::clone(&pc);
                        tokio::spawn(async move {
                            for _ in 0..5 {
                                if let Some(path) = selected_path(&pc_for_log).await {
                                    tracing::info!("session {session_id}: media path {path}");
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(300)).await;
                            }
                            tracing::info!(
                                "session {session_id}: selected candidate pair not reported"
                            );
                        });
                    }
                    RTCPeerConnectionState::Disconnected => {
                        // Possibly transient (ICE consent lost); keep the session but make
                        // sure no key stays pressed while the client is unreachable.
                        release_input(&mgr.input);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => break,
                    _ => {}
                }
            }
            PcEvent::DataChannel(dc) => {
                let label = dc.label().await.unwrap_or_default();
                tracing::info!("session {session_id}: data channel '{label}' from client");
                let input = Arc::clone(&mgr.input);
                let sh = Arc::clone(&shared);
                tokio::spawn(async move {
                    read_data_channel(dc, label, input, sh, session_id).await;
                });
            }
            PcEvent::Gathering(_) => {}
        }
    }

    // Teardown (only if a newer session has not taken over already).
    if shared.active_session.load(Ordering::Acquire) == session_id {
        shared.client_connected.store(false, Ordering::Release);
        shared.clear_sink();
        release_input(&mgr.input);
        let mut current = mgr.current.lock().await;
        if let Some(cur) = current.as_ref() {
            if Arc::ptr_eq(cur, &pc) {
                *current = None;
            }
        }
    }
    let _ = pc.close().await;
    writer.abort();
    rtcp.abort();
    tracing::info!(
        "session {session_id} ended{}",
        if connected { "" } else { " (never connected)" }
    );
}

async fn write_frames(
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    mut frames: mpsc::Receiver<crate::encoder::EncodedFrame>,
    frame_duration: Duration,
) {
    // The payload type is only known once negotiation is done, which is before the first frame
    // can arrive (frames only flow after the connection is up).
    let mut payload_type = None;
    let ssrc = match track.ssrcs().await.first() {
        Some(s) => *s,
        None => {
            tracing::error!("video track has no SSRC");
            return;
        }
    };
    let mut sent = 0u64;
    let mut last_pts: Option<u64> = None;
    while let Some(frame) = frames.recv().await {
        if payload_type.is_none() {
            match sender.get_parameters().await {
                Ok(params) => {
                    payload_type = params.rtp_parameters.codecs.first().map(|c| c.payload_type);
                }
                Err(e) => tracing::warn!("sender parameters unavailable: {e}"),
            }
        }
        let Some(pt) = payload_type else {
            tracing::warn!("no negotiated payload type yet, dropping frame");
            continue;
        };
        // Real time between frames (static content is not re-encoded every tick), bounded
        // so a long pause does not produce a huge RTP timestamp jump.
        let duration = match last_pts {
            Some(prev) if frame.pts_ms > prev => {
                Duration::from_millis(frame.pts_ms - prev).min(Duration::from_secs(2))
            }
            _ => frame_duration,
        };
        last_pts = Some(frame.pts_ms);
        let sample = Sample {
            data: Bytes::from(frame.data),
            duration,
            ..Sample::new(Instant::now())
        };
        if let Err(e) = track.sample_writer(ssrc, pt).write_sample(&sample).await {
            tracing::warn!("write_sample failed: {e}");
            break;
        }
        sent += 1;
        if frame.keyframe {
            tracing::debug!("sent keyframe #{sent} ({} bytes)", sample.data.len());
        }
    }
}

async fn read_data_channel(
    dc: Arc<dyn DataChannel>,
    label: String,
    input: SharedInput,
    shared: Arc<Shared>,
    session_id: u64,
) {
    let is_control = label == proto::CONTROL_CHANNEL;
    let is_mouse = label == proto::MOUSE_CHANNEL;
    if !is_control && !is_mouse {
        tracing::warn!("ignoring unknown data channel '{label}'");
        return;
    }
    while let Some(ev) = dc.poll().await {
        match ev {
            DataChannelEvent::OnOpen => tracing::debug!("'{label}' open"),
            DataChannelEvent::OnClose | DataChannelEvent::OnError => break,
            DataChannelEvent::OnMessage(msg) => {
                if shared.active_session.load(Ordering::Acquire) != session_id {
                    continue;
                }
                if is_control {
                    match proto::ControlMessage::decode(&msg.data) {
                        Ok(proto::ControlMessage::Hello { version }) => {
                            if version != proto::PROTOCOL_VERSION {
                                tracing::warn!(
                                    "client protocol version {version} != server {}",
                                    proto::PROTOCOL_VERSION
                                );
                            } else {
                                tracing::info!("client hello, protocol {version}");
                            }
                        }
                        Ok(proto::ControlMessage::SetBitrate { kbps }) => {
                            let kbps = kbps.clamp(200, 100_000);
                            tracing::info!("client requests {kbps} kbit/s");
                            shared.bitrate_kbps.store(kbps, Ordering::Relaxed);
                        }
                        Ok(m) => {
                            let r = input
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .handle(InputEvent::Control(m));
                            if let Err(e) = r {
                                tracing::warn!("input failed: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("bad control message: {e}"),
                    }
                } else {
                    match proto::MouseMove::decode(&msg.data) {
                        Ok(m) => {
                            let r = input
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .handle(InputEvent::Mouse(m));
                            if let Err(e) = r {
                                tracing::warn!("mouse input failed: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("bad mouse message: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
    tracing::debug!("'{label}' closed");
}
