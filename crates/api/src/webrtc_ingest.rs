// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Shared WebRTC (WHIP) ingest core.
//!
//! Terminates a WebRTC peer with webrtc-rs, forwards the publisher's RTP to
//! local UDP ports, and runs an ffmpeg subprocess that reads them via an SDP,
//! normalizes to the output canvas, and feeds FLV into the source's media relay
//! — so a WHIP publisher (OBS/hardware over H.264, or a browser over VP8)
//! becomes an ordinary, switchable Source. Both the persistent WHIP ingest
//! source (`routes::whip`) and the guest invite flow (`guest_webrtc`) call this.

use bytes::Bytes;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_remote::TrackRemote;
use webrtc::util::Marshal;

use crate::routes::output::OutputConfig;
use crate::state::AppState;

/// Whether an ingest is a persistent WHIP source or an ephemeral guest.
/// On teardown a guest's source row is deleted; a persistent source is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestKind {
    Persistent,
    Guest,
}

/// The negotiated video codec, read from the publisher's track. Determines the
/// ffmpeg SDP and the fixed payload type we rewrite forwarded RTP to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp8,
    H264,
}

/// Fixed payload types we rewrite forwarded RTP to, so the ffmpeg SDP is
/// deterministic regardless of the payload types the publisher negotiated.
const VP8_PT: u8 = 96;
const H264_PT: u8 = 97;
const OPUS_PT: u8 = 111;

/// How long a peer may sit in "disconnected" before we treat it as gone. Short
/// network blips usually recover well inside this.
const DISCONNECT_GRACE: Duration = Duration::from_secs(15);

impl VideoCodec {
    /// Map a track's negotiated mime type (e.g. "video/H264") to a codec.
    /// Anything that is not H.264 is treated as VP8.
    pub fn from_mime(mime: &str) -> VideoCodec {
        if mime.eq_ignore_ascii_case("video/H264") {
            VideoCodec::H264
        } else {
            VideoCodec::Vp8
        }
    }

    /// The fixed RTP payload type we rewrite this codec's packets to.
    pub fn payload_type(&self) -> u8 {
        match self {
            VideoCodec::Vp8 => VP8_PT,
            VideoCodec::H264 => H264_PT,
        }
    }

    fn rtpmap(&self) -> &'static str {
        match self {
            VideoCodec::Vp8 => "VP8/90000",
            VideoCodec::H264 => "H264/90000",
        }
    }
}

/// Build the ffmpeg input SDP for a given video codec and the two UDP ports the
/// publisher's RTP is forwarded to. Audio is always Opus on the fixed Opus PT.
pub fn build_ingest_sdp(video_port: u16, audio_port: u16, video: VideoCodec) -> String {
    let vpt = video.payload_type();
    let mut sdp = format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=muxshed-ingest\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=video {vp} RTP/AVP {vpt}\r\na=rtpmap:{vpt} {rtpmap}\r\n",
        vp = video_port,
        vpt = vpt,
        rtpmap = video.rtpmap(),
    );
    // H.264 over RTP needs the packetization mode so ffmpeg reassembles FU-A/STAP-A.
    if video == VideoCodec::H264 {
        sdp.push_str(&format!("a=fmtp:{} packetization-mode=1\r\n", vpt));
    }
    sdp.push_str(&format!(
        "m=audio {ap} RTP/AVP {opt}\r\na=rtpmap:{opt} opus/48000/2\r\n",
        ap = audio_port,
        opt = OPUS_PT,
    ));
    sdp
}

/// Find a free UDP port pair for the ffmpeg <- RTP bridge by binding probe
/// sockets, so we never collide with a still-running ingest's ports. The probe
/// sockets are dropped before returning, freeing the ports for ffmpeg to bind.
async fn alloc_ports() -> Result<(u16, u16), String> {
    let s1 = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("port probe: {}", e))?;
    let s2 = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("port probe: {}", e))?;
    let p1 = s1.local_addr().map_err(|e| e.to_string())?.port();
    let p2 = s2.local_addr().map_err(|e| e.to_string())?.port();
    Ok((p1, p2))
}

/// Accept a publisher's WHIP offer, wire up the RTP -> ffmpeg -> relay bridge,
/// and return the SDP answer. The source row must already exist (created by the
/// caller); on any error the caller should tear it down.
pub async fn start_ingest(
    state: Arc<AppState>,
    source_id: Uuid,
    offer_sdp: String,
    kind: IngestKind,
) -> Result<String, String> {
    let (video_port, audio_port) = alloc_ports().await?;

    let pc = build_peer(&state).await?;

    let forward = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("udp bind: {}", e))?,
    );

    // Tear the ingest down when the peer drops. "Disconnected" is often a
    // transient blip on home connections, so give it a grace period first.
    let st = state.clone();
    let this_pc = Arc::downgrade(&pc);
    pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let st = st.clone();
        let this_pc = this_pc.clone();
        Box::pin(async move {
            tracing::info!("ingest {} peer state: {:?}", source_id, s);
            match s {
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    stop_ingest_if_current(&st, &source_id, kind, &this_pc).await;
                }
                RTCPeerConnectionState::Disconnected => {
                    tokio::spawn(async move {
                        tokio::time::sleep(DISCONNECT_GRACE).await;
                        let still_down = this_pc.upgrade().is_some_and(|pc| {
                            matches!(
                                pc.connection_state(),
                                RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Failed
                            )
                        });
                        if still_down {
                            tracing::warn!(
                                "ingest {} still disconnected after {}s; tearing down",
                                source_id,
                                DISCONNECT_GRACE.as_secs()
                            );
                            stop_ingest_if_current(&st, &source_id, kind, &this_pc).await;
                        }
                    });
                }
                _ => {}
            }
        })
    }));

    let offer = RTCSessionDescription::offer(offer_sdp).map_err(|e| format!("bad offer: {}", e))?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| format!("set remote description: {}", e))?;

    let public_tx = state.get_or_create_media_relay(source_id).await;

    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create answer: {}", e))?;
    let mut gather = pc.gathering_complete_promise().await;
    pc.set_local_description(answer)
        .await
        .map_err(|e| format!("set local description: {}", e))?;
    let _ = gather.recv().await;

    let sdp = pc
        .local_description()
        .await
        .ok_or("no local description after gathering")?
        .sdp;

    // Pump: poll the transceivers' receivers for tracks and forward each one's
    // RTP. We start ffmpeg lazily once the video codec is known (see forward_track).
    let ffmpeg_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump_pc = Arc::downgrade(&pc);
    let pump_fwd = forward.clone();
    let pump_state = state.clone();
    let pump_started = ffmpeg_started.clone();
    tokio::spawn(async move {
        let mut started: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for _ in 0..60 {
            let Some(pc) = pump_pc.upgrade() else { break };
            for t in pc.get_transceivers().await {
                let is_video = t.kind() == RTPCodecType::Video;
                for track in t.receiver().await.tracks().await {
                    let ssrc = track.ssrc();
                    if ssrc == 0 || started.contains(&ssrc) {
                        continue;
                    }
                    started.insert(ssrc);
                    forward_track(
                        track,
                        is_video,
                        video_port,
                        audio_port,
                        pump_fwd.clone(),
                        source_id,
                        pump_pc.clone(),
                        pump_state.clone(),
                        public_tx.clone(),
                        pump_started.clone(),
                        kind,
                    );
                }
            }
            drop(pc);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    let watchdog_pc = Arc::downgrade(&pc);
    state.guest_peers.write().await.insert(source_id, pc);

    // Reap the ingest if it never goes live (abandoned publish, or ICE never
    // completes) so it doesn't leak an ffmpeg process, UDP ports, or peer.
    let watchdog = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let live = matches!(
            watchdog.source_states.read().await.get(&source_id),
            Some(muxshed_common::SourceState::Live)
        );
        if !live {
            tracing::warn!("ingest {} did not go live within 30s; tearing down", source_id);
            stop_ingest_if_current(&watchdog, &source_id, kind, &watchdog_pc).await;
        }
    });

    Ok(sdp)
}

/// Forward one remote track's RTP to its ffmpeg UDP port. For the first video
/// track, read the negotiated codec, start the ffmpeg bridge (once), then
/// forward, rewriting the payload type to our fixed per-codec value. For video
/// also drive periodic keyframe (PLI) requests.
#[allow(clippy::too_many_arguments)]
fn forward_track(
    track: Arc<TrackRemote>,
    is_video: bool,
    video_port: u16,
    audio_port: u16,
    fwd: Arc<UdpSocket>,
    source_id: Uuid,
    pli_pc: std::sync::Weak<RTCPeerConnection>,
    state: Arc<AppState>,
    public_tx: broadcast::Sender<Bytes>,
    ffmpeg_started: Arc<std::sync::atomic::AtomicBool>,
    kind: IngestKind,
) {
    tokio::spawn(async move {
        let kind_label = if is_video { "video" } else { "audio" };

        if is_video {
            let addr = format!("127.0.0.1:{}", video_port);

            // Read the first RTP packet BEFORE inspecting the codec: webrtc-rs only
            // populates `track.codec()` once a packet has been read, so reading it
            // earlier returns an empty mime and would mis-detect H.264 as VP8.
            let first = match track.read_rtp().await {
                Ok((packet, _)) => packet,
                Err(_) => {
                    tracing::info!("ingest {}: video track ended before first packet", source_id);
                    return;
                }
            };
            let codec = VideoCodec::from_mime(&track.codec().capability.mime_type);
            let pt = codec.payload_type();

            // Start ffmpeg once, now that the negotiated codec is known.
            if ffmpeg_started
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                tracing::info!("ingest {}: negotiated video codec {:?}", source_id, codec);
                if let Err(e) = start_ffmpeg(
                    state.clone(),
                    source_id,
                    video_port,
                    audio_port,
                    codec,
                    public_tx,
                    kind,
                    &pli_pc,
                )
                .await
                {
                    tracing::error!("ingest {}: ffmpeg start failed: {}", source_id, e);
                    stop_ingest_if_current(&state, &source_id, kind, &pli_pc).await;
                    return;
                }
            }

            // Keyframes are large, and forcing them on a schedule makes weak
            // uplinks stutter. Ask every second only until ffmpeg is decoding;
            // after that, only when packet loss was not repaired by NACK.
            let ssrc = track.ssrc();
            let pli = pli_pc.clone();
            let startup_state = state.clone();
            tokio::spawn(async move {
                for _ in 0..30 {
                    let live = matches!(
                        startup_state.source_states.read().await.get(&source_id),
                        Some(muxshed_common::SourceState::Live)
                    );
                    if live || !send_pli(&pli, ssrc).await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });

            // Forward the first packet we already read, then the rest.
            let mut n: u64 = 0;
            let mut packet = first;
            let mut loss = LossTracker::default();
            loop {
                if loss.observe(packet.header.sequence_number, std::time::Instant::now()) {
                    tracing::debug!("ingest {}: unrecovered packet loss; requesting keyframe", source_id);
                    send_pli(&pli_pc, ssrc).await;
                }
                packet.header.payload_type = pt;
                if let Ok(raw) = packet.marshal() {
                    if fwd.send_to(&raw, &addr).await.is_err() {
                        break;
                    }
                }
                n += 1;
                match track.read_rtp().await {
                    Ok((p, _)) => packet = p,
                    Err(_) => break,
                }
            }
            tracing::info!("ingest {}: video track ended after {} packets", source_id, n);
        } else {
            let addr = format!("127.0.0.1:{}", audio_port);
            let mut n: u64 = 0;
            while let Ok((mut packet, _)) = track.read_rtp().await {
                packet.header.payload_type = OPUS_PT;
                if let Ok(raw) = packet.marshal() {
                    if fwd.send_to(&raw, &addr).await.is_err() {
                        break;
                    }
                }
                n += 1;
            }
            tracing::info!("ingest {}: {} track ended after {} packets", source_id, kind_label, n);
        }
    });
}

/// Ask the publisher for a keyframe. False once the peer is gone.
async fn send_pli(pc: &Weak<RTCPeerConnection>, media_ssrc: u32) -> bool {
    let Some(pc) = pc.upgrade() else { return false };
    pc.write_rtcp(&[Box::new(PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc,
    })])
    .await
    .is_ok()
}

/// How long a lost RTP packet may stay missing before we assume NACK
/// retransmission failed and the decoder needs a keyframe.
const LOSS_REPAIR_WINDOW: Duration = Duration::from_millis(250);
/// Minimum spacing between loss-triggered keyframe requests.
const LOSS_PLI_INTERVAL: Duration = Duration::from_secs(2);
const LOSS_MAX_TRACKED: usize = 512;

/// Tracks RTP sequence gaps on one stream and decides when a keyframe request
/// is warranted: only for loss that retransmission did not fill in time.
#[derive(Default)]
struct LossTracker {
    expected: Option<u16>,
    missing: std::collections::VecDeque<(u16, std::time::Instant)>,
    last_pli: Option<std::time::Instant>,
}

impl LossTracker {
    /// Record a received sequence number; true when a keyframe should be requested.
    fn observe(&mut self, seq: u16, now: std::time::Instant) -> bool {
        match self.expected {
            None => self.expected = Some(seq.wrapping_add(1)),
            Some(exp) => {
                let ahead = seq.wrapping_sub(exp);
                if ahead == 0 {
                    self.expected = Some(seq.wrapping_add(1));
                } else if ahead < 0x8000 {
                    for i in 0..ahead.min(LOSS_MAX_TRACKED as u16) {
                        self.missing.push_back((exp.wrapping_add(i), now));
                    }
                    self.expected = Some(seq.wrapping_add(1));
                } else {
                    // Late arrival (reordered or retransmitted): no longer missing.
                    self.missing.retain(|(s, _)| *s != seq);
                }
            }
        }

        let overdue = self
            .missing
            .front()
            .is_some_and(|(_, since)| now.duration_since(*since) >= LOSS_REPAIR_WINDOW);
        if !overdue && self.missing.len() <= LOSS_MAX_TRACKED {
            return false;
        }
        self.missing.clear();
        if self.last_pli.is_some_and(|t| now.duration_since(t) < LOSS_PLI_INTERVAL) {
            return false;
        }
        self.last_pli = Some(now);
        true
    }
}

async fn build_peer(state: &AppState) -> Result<Arc<RTCPeerConnection>, String> {
    let mut m = MediaEngine::default();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            payload_type: 96,
            ..Default::default()
        },
        RTPCodecType::Video,
    )
    .map_err(|e| format!("register vp8: {}", e))?;
    // webrtc-rs matches an H.264 offer strictly by its profile-level-id, so a
    // single registered variant rejects encoders that use a different profile
    // (OBS commonly offers High profile). Register the same broad set of H.264
    // profile/packetization variants webrtc-rs uses by default, which matches
    // Chrome, Firefox, Safari, and OBS. Whatever profile is negotiated, the pump
    // reads the track's mime as H.264 and forwards it the same way.
    for (pt, fmtp) in [
        (102u8, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"),
        (127u8, "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"),
        (125u8, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
        (108u8, "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"),
        (123u8, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"),
    ] {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: fmtp.to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: pt,
                ..Default::default()
            },
            RTPCodecType::Video,
        )
        .map_err(|e| format!("register h264 {}: {}", pt, e))?;
    }
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: OPUS_PT,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )
    .map_err(|e| format!("register opus: {}", e))?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)
        .map_err(|e| format!("interceptors: {}", e))?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let ice_servers = crate::routes::webrtc_config::load(state)
        .await
        .ice_servers
        .into_iter()
        .map(|s| RTCIceServer {
            urls: s.urls,
            username: s.username.unwrap_or_default(),
            credential: s.credential.unwrap_or_default(),
        })
        .collect();
    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = api
        .new_peer_connection(config)
        .await
        .map_err(|e| format!("new peer connection: {}", e))?;

    Ok(Arc::new(pc))
}

#[allow(clippy::too_many_arguments)]
async fn start_ffmpeg(
    state: Arc<AppState>,
    source_id: Uuid,
    video_port: u16,
    audio_port: u16,
    video: VideoCodec,
    public_tx: broadcast::Sender<Bytes>,
    kind: IngestKind,
    pc: &Weak<RTCPeerConnection>,
) -> Result<(), String> {
    let cfg = load_output_config(&state).await;

    let sdp = build_ingest_sdp(video_port, audio_port, video);
    let data_dir = state.config.read().await.data_dir.clone();
    tokio::fs::create_dir_all(&data_dir).await.ok();
    let sdp_path = data_dir.join(format!("ingest-{}.sdp", source_id));
    tokio::fs::write(&sdp_path, sdp)
        .await
        .map_err(|e| format!("write sdp: {}", e))?;
    let sdp_arg = sdp_path.to_string_lossy().to_string();

    let ba = format!("{}k", cfg.audio_bitrate_kbps);
    let vf = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:black,{}",
        cfg.width,
        cfg.height,
        cfg.width,
        cfg.height,
        crate::encoder::filter_tail(&cfg)
    );

    // Always normalize onto the Studio canvas. A prior H.264 `-c:v copy` path
    // saved CPU but produced bus bitstreams that Channel HLS often decoded as
    // black while still writing a live playlist.
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
    ];
    args.extend(crate::encoder::input_args(&cfg));
    args.extend([
        "-protocol_whitelist".into(),
        "file,crypto,data,rtp,udp".into(),
        "-fflags".into(),
        "+genpts".into(),
        "-analyzeduration".into(),
        "10000000".into(),
        "-probesize".into(),
        "10000000".into(),
        "-i".into(),
        sdp_arg.clone(),
        "-vf".into(),
        vf,
    ]);
    args.extend(crate::encoder::webrtc_ingest_video_args(
        &cfg,
        &crate::encoder::Rate::canvas(&cfg),
    ));
    args.extend([
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        ba,
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-f".into(),
        "flv".into(),
        "-flvflags".into(),
        "no_duration_filesize".into(),
        "pipe:1".into(),
    ]);

    tracing::info!(
        "starting ingest ffmpeg for {} ({:?}, video :{}, audio :{})",
        source_id, video, video_port, audio_port
    );

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start ingest ffmpeg: {}", e))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;

    if let Some(stderr) = child.stderr.take() {
        let sid = source_id;
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) => break,
                    Ok(_) => tracing::debug!("ingest [{}]: {}", sid, line.trim()),
                    Err(_) => break,
                }
            }
        });
    }

    {
        // Register the ffmpeg child while holding the normalizers lock, and only
        // if this peer is still the source's current one. `stop_ingest` removes
        // the child and the peer under this same lock, so this closes the race
        // where a peer that drops (or is replaced by a reconnect) during lazy
        // startup could leave an unreaped ffmpeg child.
        let mut normalizers = state.source_normalizers.write().await;
        if !is_current_peer(&*state.guest_peers.read().await, &source_id, pc) {
            return Err("peer torn down before ffmpeg start".to_string());
        }
        if let Some(mut old) = normalizers.remove(&source_id) {
            let _ = old.kill().await;
        }
        normalizers.insert(source_id, child);
    }

    state
        .source_states
        .write()
        .await
        .insert(source_id, muxshed_common::SourceState::Connecting);
    let _ = state.ws_tx.send(muxshed_common::WsEvent::SourceState {
        id: source_id,
        state: muxshed_common::SourceState::Connecting,
    });

    tokio::spawn(async move {
        read_flv_output(source_id, stdout, public_tx, state, kind).await;
    });

    Ok(())
}

async fn read_flv_output(
    source_id: Uuid,
    mut stdout: tokio::process::ChildStdout,
    public_tx: broadcast::Sender<Bytes>,
    state: Arc<AppState>,
    kind: IngestKind,
) {
    let mut header = [0u8; 13];
    if stdout.read_exact(&mut header).await.is_err() {
        tracing::warn!("ingest: failed to read FLV header for {}", source_id);
        return;
    }

    let mut frame_count: u64 = 0;
    let mut meter =
        crate::health::RealtimeMeter::new(state.clone(), source_id, crate::health::StageKind::Ingest);

    loop {
        let mut tag_header = [0u8; 11];
        if stdout.read_exact(&mut tag_header).await.is_err() {
            break;
        }
        meter.observe(&tag_header);

        let tag_type = tag_header[0];
        let data_size = ((tag_header[1] as u32) << 16)
            | ((tag_header[2] as u32) << 8)
            | (tag_header[3] as u32);

        let mut data = vec![0u8; data_size as usize];
        if stdout.read_exact(&mut data).await.is_err() {
            break;
        }

        let mut prev_size = [0u8; 4];
        if stdout.read_exact(&mut prev_size).await.is_err() {
            break;
        }

        let total = 11 + data_size as usize + 4;
        let mut tag_buf = Vec::with_capacity(total);
        tag_buf.extend_from_slice(&tag_header);
        tag_buf.extend_from_slice(&data);
        tag_buf.extend_from_slice(&prev_size);
        let tag = Bytes::from(tag_buf);

        if tag_type == 9 && !data.is_empty() {
            let avc_packet_type = if data.len() > 1 { data[1] } else { 255 };
            let frame_type = (data[0] >> 4) & 0x0F;
            if avc_packet_type == 0 {
                let mut headers = state.sequence_headers.write().await;
                headers.entry(source_id).or_default().video = Some(tag.clone());
            } else if frame_type == 1 {
                let mut headers = state.sequence_headers.write().await;
                if let Some(entry) = headers.get_mut(&source_id) {
                    entry.last_keyframe = Some(tag.clone());
                }
            }
        } else if tag_type == 8 && !data.is_empty() {
            let aac_packet_type = if data.len() > 1 { data[1] } else { 255 };
            if aac_packet_type == 0 {
                let mut headers = state.sequence_headers.write().await;
                headers.entry(source_id).or_default().audio = Some(tag.clone());
            }
        }

        if frame_count == 0 {
            state
                .source_states
                .write()
                .await
                .insert(source_id, muxshed_common::SourceState::Live);
            let _ = state.ws_tx.send(muxshed_common::WsEvent::SourceState {
                id: source_id,
                state: muxshed_common::SourceState::Live,
            });
            if kind == IngestKind::Guest {
                let _ = sqlx::query("UPDATE guests SET status = 'connected' WHERE source_id = ?")
                    .bind(source_id.to_string())
                    .execute(&state.db)
                    .await;
            }
            tracing::info!("ingest: source {} is live", source_id);
        }

        let _ = public_tx.send(tag);
        frame_count += 1;
    }

    tracing::info!("ingest: ended for {} after {} frames", source_id, frame_count);
}

fn is_current_peer(
    peers: &std::collections::HashMap<Uuid, Arc<RTCPeerConnection>>,
    source_id: &Uuid,
    pc: &Weak<RTCPeerConnection>,
) -> bool {
    peers
        .get(source_id)
        .is_some_and(|cur| std::ptr::eq(pc.as_ptr(), Arc::as_ptr(cur)))
}

/// Tear down an ingest: stop ffmpeg, close the peer, drop the media relay and
/// sequence headers, mark the source Disconnected. The source row is kept for
/// both kinds (a guest keeps one source across reconnects so their scene
/// layers and mixer settings survive); a guest's record is marked as left.
pub async fn stop_ingest(state: &AppState, source_id: &Uuid, kind: IngestKind) {
    stop_ingest_inner(state, source_id, kind, None).await
}

/// As [`stop_ingest`], but only if `pc` is still the source's current peer.
/// Peer callbacks and watchdogs use this so a stale peer cannot tear down the
/// ingest of a publisher who has already reconnected.
async fn stop_ingest_if_current(
    state: &AppState,
    source_id: &Uuid,
    kind: IngestKind,
    pc: &Weak<RTCPeerConnection>,
) {
    stop_ingest_inner(state, source_id, kind, Some(pc)).await
}

async fn stop_ingest_inner(
    state: &AppState,
    source_id: &Uuid,
    kind: IngestKind,
    only_if_current: Option<&Weak<RTCPeerConnection>>,
) {
    // Remove the ffmpeg child and the peer under one lock (paired with the
    // current-peer check in start_ffmpeg, so a concurrent lazy start cannot
    // leak a child). Close the peer AFTER releasing the lock and off this task:
    // this also runs from the peer-state-change callback, and awaiting
    // pc.close() inline there blocks on the very callback that is running, which
    // would leave the source stuck Live. Setting Disconnected must not wait on it.
    let peer = {
        let mut normalizers = state.source_normalizers.write().await;
        let mut peers = state.guest_peers.write().await;
        if let Some(pc) = only_if_current {
            if !is_current_peer(&peers, source_id, pc) {
                return;
            }
        }
        if let Some(mut child) = normalizers.remove(source_id) {
            let _ = child.kill().await;
        }
        peers.remove(source_id)
    };
    state.remove_media_relay(source_id).await;
    state.sequence_headers.write().await.remove(source_id);
    state
        .source_states
        .write()
        .await
        .insert(*source_id, muxshed_common::SourceState::Disconnected);
    let _ = state.ws_tx.send(muxshed_common::WsEvent::SourceState {
        id: *source_id,
        state: muxshed_common::SourceState::Disconnected,
    });

    if kind == IngestKind::Guest {
        let _ = sqlx::query("UPDATE guests SET status = 'left' WHERE source_id = ?")
            .bind(source_id.to_string())
            .execute(&state.db)
            .await;
    }

    let data_dir = state.config.read().await.data_dir.clone();
    let _ = tokio::fs::remove_file(data_dir.join(format!("ingest-{}.sdp", source_id))).await;

    // Drop any WHIP session pointing at this source so the map does not grow on
    // callback/watchdog teardown paths that never hit DELETE.
    state
        .whip_sessions
        .write()
        .await
        .retain(|_, sid| sid != source_id);

    // Close the peer last, off this task, so a callback-context caller does not
    // block on its own close.
    if let Some(pc) = peer {
        tokio::spawn(async move {
            let _ = pc.close().await;
        });
    }
}

async fn load_output_config(state: &AppState) -> OutputConfig {
    sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = 'output_config'")
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|(json,)| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_mime_maps_h264_and_defaults_vp8() {
        assert_eq!(VideoCodec::from_mime("video/H264"), VideoCodec::H264);
        assert_eq!(VideoCodec::from_mime("video/h264"), VideoCodec::H264);
        assert_eq!(VideoCodec::from_mime("video/VP8"), VideoCodec::Vp8);
        assert_eq!(VideoCodec::from_mime(""), VideoCodec::Vp8);
    }

    #[test]
    fn sdp_h264_has_rtpmap_and_packetization_mode() {
        let sdp = build_ingest_sdp(5000, 5002, VideoCodec::H264);
        assert!(sdp.contains("m=video 5000 RTP/AVP 97"));
        assert!(sdp.contains("a=rtpmap:97 H264/90000"));
        assert!(sdp.contains("a=fmtp:97 packetization-mode=1"));
        assert!(sdp.contains("m=audio 5002 RTP/AVP 111"));
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
    }

    #[test]
    fn loss_in_order_stream_never_requests_keyframe() {
        let mut t = LossTracker::default();
        let start = std::time::Instant::now();
        for i in 0..2000u32 {
            let seq = (65000u32 + i) as u16; // crosses the u16 wrap
            assert!(!t.observe(seq, start + Duration::from_millis(i as u64 * 10)));
        }
    }

    #[test]
    fn loss_repaired_by_retransmission_is_ignored() {
        let mut t = LossTracker::default();
        let t0 = std::time::Instant::now();
        assert!(!t.observe(10, t0));
        assert!(!t.observe(12, t0)); // 11 missing
        assert!(!t.observe(11, t0 + Duration::from_millis(50))); // NACK fill
        assert!(!t.observe(13, t0 + Duration::from_millis(400)));
    }

    #[test]
    fn unrepaired_loss_requests_keyframe_rate_limited() {
        let mut t = LossTracker::default();
        let t0 = std::time::Instant::now();
        t.observe(10, t0);
        t.observe(12, t0); // 11 never arrives
        assert!(t.observe(13, t0 + Duration::from_millis(300)));
        t.observe(15, t0 + Duration::from_millis(400)); // 14 lost too
        assert!(!t.observe(16, t0 + Duration::from_millis(800))); // within 2s
        t.observe(18, t0 + Duration::from_millis(2500));
        assert!(t.observe(19, t0 + Duration::from_millis(2800)));
    }

    #[test]
    fn sdp_vp8_has_no_packetization_mode() {
        let sdp = build_ingest_sdp(6000, 6002, VideoCodec::Vp8);
        assert!(sdp.contains("m=video 6000 RTP/AVP 96"));
        assert!(sdp.contains("a=rtpmap:96 VP8/90000"));
        assert!(!sdp.contains("packetization-mode"));
    }
}
