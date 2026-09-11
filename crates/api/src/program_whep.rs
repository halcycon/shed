// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Low-latency studio monitors via WHEP (WebRTC-HTTP Egress Protocol).
//!
//! One ffmpeg encoder per feed (Program bus or a single source relay) reads FLV
//! tags, re-encodes to VP8 + Opus RTP, and fans into send-only peer connections.
//!
//! - `POST /api/v1/program/whep` — Program bus
//! - `POST /api/v1/sources/{id}/whep` — one source (Preview / selected monitor)
//!
//! Source *thumbnails* should stay on WS-FLV to avoid dozens of encoders.

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::util::Unmarshal;

use crate::rtmp::flv;
use crate::state::{AppState, SequenceHeaders};

const FLV_TAG_VIDEO: u8 = 9;
const VP8_PT: u8 = 96;
const OPUS_PT: u8 = 111;

fn is_video_keyframe(data: &[u8]) -> bool {
    if data.len() < 13 || data[0] != FLV_TAG_VIDEO {
        return false;
    }
    let frame_type = data[11] & 0xF0;
    let is_seq_header = data[12] == 0x00;
    frame_type == 0x10 && !is_seq_header
}

fn is_video_sequence_header(data: &[u8]) -> bool {
    data.len() >= 13 && data[0] == FLV_TAG_VIDEO && data[12] == 0x00
}

fn is_audio_sequence_header(data: &[u8]) -> bool {
    data.len() >= 13 && data[0] == 8 && data[12] == 0x00
}

struct SharedTracks {
    video: Arc<TrackLocalStaticRTP>,
    audio: Arc<TrackLocalStaticRTP>,
}

struct Encoder {
    child: Child,
    _cancel: tokio::sync::watch::Sender<bool>,
}

struct Session {
    pc: Arc<RTCPeerConnection>,
}

/// One FLV→VP8/Opus WHEP encoder shared by N viewers of the same feed.
pub struct FlvWhepHub {
    label: String,
    sessions: Mutex<HashMap<Uuid, Session>>,
    tracks: Mutex<Option<SharedTracks>>,
    encoder: Mutex<Option<Encoder>>,
}

impl FlvWhepHub {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            sessions: Mutex::new(HashMap::new()),
            tracks: Mutex::new(None),
            encoder: Mutex::new(None),
        }
    }

    pub async fn create_session(
        self: &Arc<Self>,
        state: Arc<AppState>,
        offer_sdp: String,
        media_tx: broadcast::Sender<Bytes>,
        seq: Option<SequenceHeaders>,
    ) -> Result<(Uuid, String), String> {
        let tracks = self
            .ensure_encoder(state.clone(), media_tx, seq)
            .await?;

        let pc = build_send_peer(&state).await?;
        pc.add_track(Arc::clone(&tracks.video) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| format!("add video track: {e}"))?;
        pc.add_track(Arc::clone(&tracks.audio) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .map_err(|e| format!("add audio track: {e}"))?;

        let session_id = Uuid::new_v4();
        let hub = Arc::clone(self);
        let sid = session_id;
        let label = self.label.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let hub = Arc::clone(&hub);
            let label = label.clone();
            Box::pin(async move {
                if matches!(
                    s,
                    RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Disconnected
                ) {
                    tracing::info!("WHEP [{}] session {} state {:?}", label, sid, s);
                    let _ = hub.delete_session(sid).await;
                }
            })
        }));

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| format!("parse offer: {e}"))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| format!("set remote: {e}"))?;

        let answer = pc
            .create_answer(None)
            .await
            .map_err(|e| format!("create answer: {e}"))?;
        let mut gather = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .map_err(|e| format!("set local: {e}"))?;
        let _ = gather.recv().await;

        let local = pc
            .local_description()
            .await
            .ok_or_else(|| "no local description".to_string())?;

        self.sessions.lock().await.insert(
            session_id,
            Session {
                pc: Arc::clone(&pc),
            },
        );
        tracing::info!("WHEP [{}] session {} created", self.label, session_id);
        Ok((session_id, local.sdp))
    }

    pub async fn delete_session(&self, session_id: Uuid) -> bool {
        let removed = self.sessions.lock().await.remove(&session_id);
        let existed = removed.is_some();
        if let Some(session) = removed {
            let _ = session.pc.close().await;
            tracing::info!("WHEP [{}] session {} closed", self.label, session_id);
        }
        if self.sessions.lock().await.is_empty() {
            self.stop_encoder().await;
        }
        existed
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    async fn ensure_encoder(
        self: &Arc<Self>,
        _state: Arc<AppState>,
        media_tx: broadcast::Sender<Bytes>,
        seq: Option<SequenceHeaders>,
    ) -> Result<SharedTracks, String> {
        {
            let tracks = self.tracks.lock().await;
            if let Some(t) = tracks.as_ref() {
                return Ok(SharedTracks {
                    video: Arc::clone(&t.video),
                    audio: Arc::clone(&t.audio),
                });
            }
        }

        let mut enc = self.encoder.lock().await;
        let mut tracks_guard = self.tracks.lock().await;
        if let Some(t) = tracks_guard.as_ref() {
            return Ok(SharedTracks {
                video: Arc::clone(&t.video),
                audio: Arc::clone(&t.audio),
            });
        }

        let video_sock = UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind video udp: {e}"))?;
        let audio_sock = UdpSocket::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind audio udp: {e}"))?;
        let video_port = video_sock.local_addr().map_err(|e| e.to_string())?.port();
        let audio_port = audio_sock.local_addr().map_err(|e| e.to_string())?.port();

        let stream_id = format!("muxshed-{}", self.label);
        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            stream_id.clone(),
        ));
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            "audio".to_owned(),
            stream_id,
        ));

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        spawn_rtp_pump(
            video_sock,
            Arc::clone(&video_track),
            cancel_rx.clone(),
            "video",
        );
        spawn_rtp_pump(
            audio_sock,
            Arc::clone(&audio_track),
            cancel_rx.clone(),
            "audio",
        );

        let mut child = start_monitor_ffmpeg(video_port, audio_port).await?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "no ffmpeg stdin".to_string())?;

        let label = self.label.clone();
        let mut cancel_feed = cancel_rx.clone();
        tokio::spawn(async move {
            feed_flv(stdin, media_tx, seq, &mut cancel_feed, &label).await;
        });

        if let Some(stderr) = child.stderr.take() {
            let label = self.label.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("ffmpeg [whep:{}]: {}", label, line);
                }
            });
        }

        tracing::info!(
            "WHEP [{}] encoder started (VP8 :{}, Opus :{})",
            self.label,
            video_port,
            audio_port
        );

        let shared = SharedTracks {
            video: Arc::clone(&video_track),
            audio: Arc::clone(&audio_track),
        };
        *tracks_guard = Some(SharedTracks {
            video: video_track,
            audio: audio_track,
        });
        *enc = Some(Encoder {
            child,
            _cancel: cancel_tx,
        });

        Ok(shared)
    }

    async fn stop_encoder(&self) {
        if let Some(mut enc) = self.encoder.lock().await.take() {
            let _ = enc._cancel.send(true);
            let _ = enc.child.kill().await;
            tracing::info!("WHEP [{}] encoder stopped", self.label);
        }
        *self.tracks.lock().await = None;
    }
}

/// Program-bus WHEP (one shared hub).
pub struct ProgramWhep {
    hub: Arc<FlvWhepHub>,
}

impl ProgramWhep {
    pub fn new() -> Self {
        Self {
            hub: Arc::new(FlvWhepHub::new("program")),
        }
    }

    pub async fn create_session(
        &self,
        state: Arc<AppState>,
        offer_sdp: String,
    ) -> Result<(Uuid, String), String> {
        let seq = current_program_headers(&state).await;
        let media_tx = state.program_tx.clone();
        self.hub
            .create_session(state, offer_sdp, media_tx, seq)
            .await
    }

    pub async fn delete_session(&self, session_id: Uuid) -> bool {
        self.hub.delete_session(session_id).await
    }
}

impl Default for ProgramWhep {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-source WHEP hubs (Preview / selected monitors — not thumbnail grids).
pub struct SourceWhepRegistry {
    hubs: Mutex<HashMap<Uuid, Arc<FlvWhepHub>>>,
}

impl SourceWhepRegistry {
    pub fn new() -> Self {
        Self {
            hubs: Mutex::new(HashMap::new()),
        }
    }

    pub async fn create_session(
        &self,
        state: Arc<AppState>,
        source_id: Uuid,
        offer_sdp: String,
    ) -> Result<(Uuid, String), String> {
        let media_tx = state
            .get_media_relay(&source_id)
            .await
            .ok_or_else(|| format!("no media relay for source {source_id}"))?;

        let seq = state
            .sequence_headers
            .read()
            .await
            .get(&source_id)
            .cloned();

        let hub = {
            let mut hubs = self.hubs.lock().await;
            hubs.entry(source_id)
                .or_insert_with(|| Arc::new(FlvWhepHub::new(format!("source-{source_id}"))))
                .clone()
        };

        hub.create_session(state, offer_sdp, media_tx, seq).await
    }

    pub async fn delete_session(&self, source_id: Uuid, session_id: Uuid) -> bool {
        let hub = {
            let hubs = self.hubs.lock().await;
            hubs.get(&source_id).cloned()
        };
        let Some(hub) = hub else {
            return false;
        };
        let ok = hub.delete_session(session_id).await;
        // Drop idle hubs so we do not leak encoder slots.
        if hub.session_count().await == 0 {
            self.hubs.lock().await.remove(&source_id);
        }
        ok
    }
}

impl Default for SourceWhepRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_rtp_pump(
    sock: UdpSocket,
    track: Arc<TrackLocalStaticRTP>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    label: &'static str,
) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                _ = cancel.changed() => {
                    if *cancel.borrow() { break; }
                }
                res = sock.recv(&mut buf) => {
                    match res {
                        Ok(n) => {
                            let mut b = BytesMut::from(&buf[..n]);
                            match Packet::unmarshal(&mut b) {
                                Ok(pkt) => {
                                    if let Err(e) = track.write_rtp(&pkt).await {
                                        tracing::debug!("WHEP {} write_rtp: {}", label, e);
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("WHEP {} rtp parse: {}", label, e);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("WHEP {} udp recv: {}", label, e);
                            break;
                        }
                    }
                }
            }
        }
    });
}

async fn start_monitor_ffmpeg(video_port: u16, audio_port: u16) -> Result<Child, String> {
    let vurl = format!("rtp://127.0.0.1:{}?pkt_size=1200", video_port);
    let aurl = format!("rtp://127.0.0.1:{}?pkt_size=1200", audio_port);
    let args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-fflags".into(),
        "nobuffer".into(),
        "-flags".into(),
        "low_delay".into(),
        "-f".into(),
        "flv".into(),
        "-i".into(),
        "pipe:0".into(),
        "-map".into(),
        "0:v:0".into(),
        "-vf".into(),
        "scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease".into(),
        "-c:v".into(),
        "libvpx".into(),
        "-deadline".into(),
        "realtime".into(),
        "-cpu-used".into(),
        "8".into(),
        "-b:v".into(),
        "1200k".into(),
        "-g".into(),
        "30".into(),
        "-an".into(),
        "-payload_type".into(),
        VP8_PT.to_string(),
        "-f".into(),
        "rtp".into(),
        vurl,
        "-map".into(),
        "0:a:0?".into(),
        "-c:a".into(),
        "libopus".into(),
        "-application".into(),
        "lowdelay".into(),
        "-b:a".into(),
        "96k".into(),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "2".into(),
        "-vn".into(),
        "-payload_type".into(),
        OPUS_PT.to_string(),
        "-f".into(),
        "rtp".into(),
        aurl,
    ];

    Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("start WHEP ffmpeg: {e}"))
}

async fn current_program_headers(state: &AppState) -> Option<SequenceHeaders> {
    let program_source = *state.program_source.borrow();
    let audio_routing = state.audio_routing.borrow().clone();
    let Some(video_id) = program_source else {
        tracing::info!("WHEP [program]: no program source yet");
        return None;
    };
    let audio_id =
        crate::program::resolve_program_audio_source(state, video_id, &audio_routing).await;
    tracing::info!(
        "WHEP [program]: effective source {} (audio {})",
        video_id,
        audio_id
    );
    let headers = state.sequence_headers.read().await;
    let video_h = headers.get(&video_id);
    let audio_h = headers.get(&audio_id);
    Some(SequenceHeaders {
        video: video_h.and_then(|h| h.video.clone()),
        audio: audio_h
            .and_then(|h| h.audio.clone())
            .or_else(|| video_h.and_then(|h| h.audio.clone())),
        last_keyframe: video_h.and_then(|h| h.last_keyframe.clone()),
    })
}

async fn feed_flv(
    mut stdin: tokio::process::ChildStdin,
    media_tx: broadcast::Sender<Bytes>,
    sequence_headers: Option<SequenceHeaders>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    label: &str,
) {
    let started = Instant::now();
    let header = flv::flv_header();
    if stdin.write_all(&header).await.is_err() {
        return;
    }
    if let Some(ref seq) = sequence_headers {
        if let Some(ref video) = seq.video {
            let _ = stdin.write_all(video).await;
        }
        if let Some(ref audio) = seq.audio {
            let _ = stdin.write_all(audio).await;
        }
        if let Some(ref kf) = seq.last_keyframe {
            let _ = stdin.write_all(kf).await;
        }
    }

    let mut rx = media_tx.subscribe();
    tracing::info!("WHEP [{}]: waiting for first keyframe", label);
    let mut waiting = true;

    loop {
        tokio::select! {
            _ = cancel.changed() => {
                if *cancel.borrow() { break; }
            }
            msg = rx.recv() => {
                match msg {
                    Ok(data) => {
                        if waiting {
                            if is_video_sequence_header(&data) || is_audio_sequence_header(&data) {
                                if stdin.write_all(&data).await.is_err() { break; }
                                continue;
                            }
                            if data.first() == Some(&FLV_TAG_VIDEO) && is_video_keyframe(&data) {
                                waiting = false;
                                tracing::info!(
                                    "WHEP [{}]: first keyframe after {} ms",
                                    label,
                                    started.elapsed().as_millis()
                                );
                            } else {
                                continue;
                            }
                        } else if is_video_sequence_header(&data) {
                            waiting = true;
                            tracing::info!("WHEP [{}]: new video seq; waiting for keyframe", label);
                            if stdin.write_all(&data).await.is_err() { break; }
                            continue;
                        }
                        if stdin.write_all(&data).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WHEP [{}] lagged {} packets; waiting for keyframe", label, n);
                        waiting = true;
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn build_send_peer(state: &AppState) -> Result<Arc<RTCPeerConnection>, String> {
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
            payload_type: VP8_PT,
            ..Default::default()
        },
        RTPCodecType::Video,
    )
    .map_err(|e| format!("register vp8: {e}"))?;
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
    .map_err(|e| format!("register opus: {e}"))?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)
        .map_err(|e| format!("interceptors: {e}"))?;
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

    let pc = api
        .new_peer_connection(RTCConfiguration {
            ice_servers,
            ..Default::default()
        })
        .await
        .map_err(|e| format!("new peer: {e}"))?;
    Ok(Arc::new(pc))
}
