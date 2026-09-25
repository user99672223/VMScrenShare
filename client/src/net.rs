//! WebRTC client session: signalling over HTTP, one incoming H.264 track, two data channels.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Bytes, BytesMut};
use proto::signalling::{ErrorResponse, SessionDescription, OFFER_PATH};
use proto::{ControlMessage, MouseMove, CONTROL_CHANNEL, MOUSE_CHANNEL, PROTOCOL_VERSION};
use rtc::data_channel::RTCDataChannelInit;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::ice::network_type::NetworkType;
use rtc::media::io::sample_builder::SampleBuilder;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use webrtc::data_channel::DataChannelEvent;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceConnectionState,
    RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription, Registry,
    SettingEngineBuilder,
};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;

pub struct NetConfig {
    pub server_url: String,
    pub bitrate_kbps: Option<u32>,
}

/// UI thread → network.
#[derive(Debug)]
pub enum UiCommand {
    Control(ControlMessage),
    Mouse(MouseMove),
    Quit,
}

/// Decoder thread → network.
#[derive(Debug)]
pub enum DecoderRequest {
    /// Ask the server for an IDR frame (PLI).
    Keyframe,
}

enum PcEvent {
    Connection(RTCPeerConnectionState),
    Ice(RTCIceConnectionState),
    Gathering(RTCIceGatheringState),
    Track(Arc<dyn TrackRemote>),
}

struct Handler {
    events: UnboundedSender<PcEvent>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        let _ = self.events.send(PcEvent::Gathering(state));
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        let _ = self.events.send(PcEvent::Ice(state));
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.events.send(PcEvent::Connection(state));
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.events.send(PcEvent::Track(track));
    }
}

const GATHER_TIMEOUT: Duration = Duration::from_secs(3);
/// Minimum spacing between PLIs.
const PLI_INTERVAL: Duration = Duration::from_millis(250);
/// Mouse positions are coalesced and flushed at this rate.
const MOUSE_FLUSH: Duration = Duration::from_millis(4);

fn status(proxy: &EventLoopProxy<UserEvent>, text: impl Into<String>) {
    let _ = proxy.send_event(UserEvent::Status(text.into()));
}

fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: "video/H264".to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
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
        payload_type: 102,
    }
}

pub async fn run(
    cfg: NetConfig,
    mut ui_rx: UnboundedReceiver<UiCommand>,
    au_tx: SyncSender<Bytes>,
    mut req_rx: UnboundedReceiver<DecoderRequest>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    status(&proxy, "connecting");

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(h264_codec(), RtpCodecKind::Video)
        .context("registering H264")?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .context("default interceptors")?;
    // Dual stack: gather IPv4 and IPv6 host candidates; ICE picks whichever path works.
    let setting_engine = SettingEngineBuilder::new()
        .with_network_types(vec![NetworkType::Udp4, NetworkType::Udp6])
        .with_multicast_dns_mode(MulticastDnsMode::Disabled)
        .build();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_setting_engine(setting_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { events: events_tx }))
        .with_udp_addrs(vec!["0.0.0.0:0".to_string(), "[::]:0".to_string()])
        .build()
        .await
        .context("creating peer connection")?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    let control = pc
        .create_data_channel(
            CONTROL_CHANNEL,
            Some(RTCDataChannelInit {
                ordered: true,
                ..Default::default()
            }),
        )
        .await
        .context("creating control channel")?;
    let mouse = pc
        .create_data_channel(
            MOUSE_CHANNEL,
            Some(RTCDataChannelInit {
                ordered: false,
                max_retransmits: Some(0),
                ..Default::default()
            }),
        )
        .await
        .context("creating mouse channel")?;
    pc.add_transceiver_from_kind(
        RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            ..Default::default()
        }),
    )
    .await
    .context("adding video transceiver")?;

    let offer = pc.create_offer(None).await.context("create_offer")?;
    pc.set_local_description(offer)
        .await
        .context("set_local_description")?;

    // Non-trickle: wait for gathering to finish (host candidates only, so this is quick).
    let mut pending = Vec::new();
    let deadline = Instant::now() + GATHER_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline.into(), events_rx.recv()).await {
            Ok(Some(PcEvent::Gathering(RTCIceGatheringState::Complete))) => break,
            Ok(Some(PcEvent::Gathering(_))) => {}
            Ok(Some(ev)) => pending.push(ev),
            Ok(None) => bail!("peer connection closed during gathering"),
            Err(_) => {
                tracing::warn!(
                    "ICE gathering did not finish in {GATHER_TIMEOUT:?}, sending anyway"
                );
                break;
            }
        }
    }
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description"))?;

    status(&proxy, "signalling");
    let answer_sdp = exchange_offer(&cfg.server_url, local.sdp).await?;
    tracing::info!(
        "answer: {} candidate(s) {:?}, ice-lite {}",
        proto::sdp::candidate_count(&answer_sdp),
        proto::sdp::host_candidate_addresses(&answer_sdp),
        proto::sdp::is_ice_lite(&answer_sdp)
    );
    let answer = RTCSessionDescription::answer(answer_sdp).context("parsing answer")?;
    pc.set_remote_description(answer)
        .await
        .context("set_remote_description")?;
    status(&proxy, "connecting (ICE)");

    // Data channel event loops.
    let hello = ControlMessage::Hello {
        version: PROTOCOL_VERSION,
    };
    let initial_bitrate = cfg.bitrate_kbps;
    {
        let control = Arc::clone(&control);
        tokio::spawn(async move {
            while let Some(ev) = control.poll().await {
                match ev {
                    DataChannelEvent::OnOpen => {
                        tracing::info!("control channel open");
                        let _ = control.send(BytesMut::from(&hello.encode()[..])).await;
                        if let Some(kbps) = initial_bitrate {
                            let msg = ControlMessage::SetBitrate { kbps };
                            let _ = control.send(BytesMut::from(&msg.encode()[..])).await;
                        }
                    }
                    DataChannelEvent::OnClose | DataChannelEvent::OnError => {
                        tracing::info!("control channel closed");
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
    {
        let mouse = Arc::clone(&mouse);
        tokio::spawn(async move {
            while let Some(ev) = mouse.poll().await {
                match ev {
                    DataChannelEvent::OnOpen => tracing::info!("mouse channel open"),
                    DataChannelEvent::OnClose | DataChannelEvent::OnError => break,
                    _ => {}
                }
            }
        });
    }

    let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
    let mut state = LoopState {
        video: None,
        connected: false,
    };
    let mut last_pli = Instant::now() - PLI_INTERVAL;
    let mut latest_mouse: Option<MouseMove> = None;
    let mut mouse_tick = tokio::time::interval(MOUSE_FLUSH);
    mouse_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    for ev in pending.drain(..) {
        if on_event(ev, &mut state, &pc, &proxy, &au_tx, &kf_tx).await? {
            if let Some((track, ssrc)) = &state.video {
                send_pli(track, *ssrc).await;
                last_pli = Instant::now();
            }
        }
    }

    loop {
        tokio::select! {
            ev = events_rx.recv() => {
                match ev {
                    Some(ev) => {
                        if on_event(ev, &mut state, &pc, &proxy, &au_tx, &kf_tx).await? {
                            // New track: ask for a keyframe right away.
                            if let Some((track, ssrc)) = &state.video {
                                send_pli(track, *ssrc).await;
                                last_pli = Instant::now();
                            }
                        }
                    }
                    None => bail!("peer connection driver stopped"),
                }
            }
            cmd = ui_rx.recv() => {
                match cmd {
                    Some(UiCommand::Control(msg)) => {
                        if let Err(e) = control.send(BytesMut::from(&msg.encode()[..])).await {
                            tracing::debug!("control send failed: {e}");
                        }
                    }
                    Some(UiCommand::Mouse(m)) => latest_mouse = Some(m),
                    Some(UiCommand::Quit) | None => break,
                }
            }
            _ = mouse_tick.tick() => {
                if let Some(m) = latest_mouse.take() {
                    if let Err(e) = mouse.try_send(BytesMut::from(&m.encode()[..])).await {
                        tracing::trace!("mouse send failed: {e}");
                    }
                }
            }
            req = req_rx.recv() => {
                if let Some(DecoderRequest::Keyframe) = req {
                    if let Some((track, ssrc)) = &state.video {
                        if last_pli.elapsed() >= PLI_INTERVAL {
                            send_pli(track, *ssrc).await;
                            last_pli = Instant::now();
                        }
                    }
                }
            }
            _ = kf_rx.recv() => {
                if let Some((track, ssrc)) = &state.video {
                    if last_pli.elapsed() >= PLI_INTERVAL {
                        send_pli(track, *ssrc).await;
                        last_pli = Instant::now();
                    }
                }
            }
        }
    }

    let _ = control
        .send(BytesMut::from(&ControlMessage::ReleaseAll.encode()[..]))
        .await;
    let _ = pc.close().await;
    Ok(())
}

struct LoopState {
    video: Option<(Arc<dyn TrackRemote>, u32)>,
    connected: bool,
}

/// Handles one peer-connection event. Returns `true` when a new video track appeared.
async fn on_event(
    ev: PcEvent,
    state: &mut LoopState,
    pc: &Arc<dyn PeerConnection>,
    proxy: &EventLoopProxy<UserEvent>,
    au_tx: &SyncSender<Bytes>,
    kf_tx: &UnboundedSender<()>,
) -> Result<bool> {
    match ev {
        PcEvent::Connection(pc_state) => {
            tracing::info!("connection state: {pc_state}");
            match pc_state {
                RTCPeerConnectionState::Connected => {
                    state.connected = true;
                    status(proxy, "connected");
                    // Report the nominated path once ICE exposes it (usually immediately).
                    let pc = Arc::clone(pc);
                    let proxy = proxy.clone();
                    tokio::spawn(async move {
                        for _ in 0..5 {
                            if let Some(path) = selected_path(&pc).await {
                                tracing::info!("media path {path}");
                                status(&proxy, format!("connected via {path}"));
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                    });
                }
                RTCPeerConnectionState::Disconnected => status(proxy, "disconnected (waiting)"),
                RTCPeerConnectionState::Failed => {
                    bail!(if state.connected {
                        "connection lost (ICE failed)"
                    } else {
                        "could not connect: ICE failed. Check the OCI security list and iptables (UDP 50000-50100) and run `server doctor` on the VM"
                    })
                }
                RTCPeerConnectionState::Closed => bail!("connection closed by server"),
                _ => {}
            }
        }
        PcEvent::Ice(ice) => tracing::debug!("ICE connection state: {ice}"),
        PcEvent::Gathering(_) => {}
        PcEvent::Track(track) => {
            let ssrc = track.ssrcs().await.first().copied();
            tracing::info!("video track (ssrc {ssrc:?})");
            if let Some(ssrc) = ssrc {
                state.video = Some((Arc::clone(&track), ssrc));
            }
            tokio::spawn(read_track(track, au_tx.clone(), kf_tx.clone()));
            return Ok(true);
        }
    }
    Ok(false)
}

/// `"IPv6 to [2001:db8::1]:50000"` for the nominated candidate pair, if known yet.
async fn selected_path(pc: &Arc<dyn PeerConnection>) -> Option<String> {
    let sctp = pc.sctp().await?;
    let ice = sctp.transport().ice_transport();
    let pair = ice.get_selected_candidate_pair().await.ok().flatten()?;
    let remote = pair.remote();
    Some(format!(
        "{} to {}",
        proto::sdp::address_family(&remote.address),
        proto::sdp::format_endpoint(&remote.address, remote.port)
    ))
}

async fn send_pli(track: &Arc<dyn TrackRemote>, ssrc: u32) {
    let pli = PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc: ssrc,
    };
    if let Err(e) = track.write_rtcp(vec![Box::new(pli)]).await {
        tracing::debug!("PLI failed: {e}");
    } else {
        tracing::debug!("PLI sent");
    }
}

/// Reassembles access units from RTP and forwards them to the decoder thread.
async fn read_track(
    track: Arc<dyn TrackRemote>,
    au_tx: SyncSender<Bytes>,
    keyframe: UnboundedSender<()>,
) {
    let mut builder = SampleBuilder::new(64, H264Packet::default(), 90_000)
        .with_max_time_delay(Duration::from_millis(500));
    let mut samples = 0u64;
    while let Some(ev) = track.poll().await {
        match ev {
            TrackRemoteEvent::OnRtpPacket(packet) => {
                let now = Instant::now();
                builder.push(now, packet);
                while let Some(sample) = builder.pop(now) {
                    samples += 1;
                    match au_tx.try_send(sample.data) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            tracing::warn!(
                                "decoder queue full, dropping frame and requesting a keyframe"
                            );
                            let _ = keyframe.send(());
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
            TrackRemoteEvent::OnEnded => break,
            _ => {}
        }
    }
    tracing::info!("video track ended after {samples} samples");
}

async fn exchange_offer(server_url: &str, offer_sdp: String) -> Result<String> {
    let url = format!("{}{}", server_url.trim_end_matches('/'), OFFER_PATH);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()
        .context("building HTTP client")?;
    let response = client
        .post(&url)
        .json(&SessionDescription::offer(offer_sdp))
        .send()
        .await
        .with_context(|| {
            format!("POST {url} failed. Is the SSH tunnel up? (ssh -L 8080:127.0.0.1:8080 <vm>)")
        })?;
    let status_code = response.status();
    let body = response.text().await.context("reading answer")?;
    if !status_code.is_success() {
        let detail = serde_json::from_str::<ErrorResponse>(&body)
            .map(|e| e.error)
            .unwrap_or(body);
        bail!("server rejected the offer ({status_code}): {detail}");
    }
    let answer: SessionDescription = serde_json::from_str(&body).context("parsing answer JSON")?;
    if answer.kind != "answer" {
        bail!("expected an answer, got type {:?}", answer.kind);
    }
    Ok(answer.sdp)
}
