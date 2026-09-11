// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use bytes::Bytes;
use muxshed_common::{MuxshedConfig, SourceState, WsEvent};
use muxshed_processor::PipelineController;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, RwLock};
use uuid::Uuid;

use crate::channel_hls::ChannelHls;
use crate::egress::EgressManager;

pub struct AppState {
    pub pipeline: Arc<dyn PipelineController>,
    pub config: Arc<RwLock<MuxshedConfig>>,
    pub db: SqlitePool,
    pub ws_tx: broadcast::Sender<WsEvent>,
    pub source_states: RwLock<HashMap<Uuid, SourceState>>,
    pub media_relays: RwLock<HashMap<Uuid, broadcast::Sender<Bytes>>>,
    pub sequence_headers: RwLock<HashMap<Uuid, SequenceHeaders>>,
    pub source_media_info: RwLock<HashMap<Uuid, SourceMediaInfo>>,
    pub media_players: RwLock<HashMap<Uuid, tokio::process::Child>>,
    pub source_normalizers: RwLock<HashMap<Uuid, tokio::process::Child>>,
    pub srt_listeners: RwLock<HashMap<Uuid, tokio::process::Child>>,
    /// Live WebRTC guest peer connections, kept alive while the guest is connected
    pub guest_peers: RwLock<HashMap<Uuid, Arc<webrtc::peer_connection::RTCPeerConnection>>>,
    /// WHIP publish sessions: session id -> source id, for DELETE teardown.
    pub whip_sessions: RwLock<HashMap<Uuid, Uuid>>,
    /// Scene compositors (ffmpeg) keyed by scene id — composite a scene's layers
    pub scene_compositors: RwLock<HashMap<Uuid, tokio::process::Child>>,
    /// Cancel signals for identity scene forwarders (single full-frame layer, no ffmpeg)
    pub scene_forward_cancels: RwLock<HashMap<Uuid, watch::Sender<bool>>>,
    /// Headless Chrome processes for browser sources
    pub browser_sources: RwLock<HashMap<Uuid, tokio::process::Child>>,
    pub egress: EgressManager,
    /// Public Channel HLS output (ffmpeg) for the watch page
    pub channel_hls: ChannelHls,
    /// Low-latency Program WHEP monitor hub
    pub program_whep: Arc<crate::program_whep::ProgramWhep>,
    /// Per-source WHEP hubs (Preview / selected monitors)
    pub source_whep: Arc<crate::program_whep::SourceWhepRegistry>,
    /// Program output — the channel that egress and preview-program read from
    pub program_tx: broadcast::Sender<Bytes>,
    /// The source actually routed to program. Written by the failover supervisor,
    /// read by the program router. During a failover this is the fallback source.
    pub program_source: watch::Sender<Option<Uuid>>,
    /// The operator's selected program source. The failover supervisor mirrors
    /// this to `program_source` when it is live, or substitutes the fallback when
    /// it goes offline. All operator-facing switches set this, not program_source.
    pub program_intent: watch::Sender<Option<Uuid>>,
    /// Whether a failover is currently engaged (program is on the fallback).
    pub failover_active: watch::Sender<bool>,
    /// The schedule whose broadcast is currently on air (None = manual/idle).
    pub active_schedule: watch::Sender<Option<Uuid>>,
    /// Bumped whenever schedules or the system timezone change, to wake the scheduler.
    pub schedule_nudge: watch::Sender<u64>,
    /// Which source is in preview (next to go live)
    pub preview_source: RwLock<Option<Uuid>>,
    /// Audio routing: which sources are providing audio and at what level
    pub audio_routing: watch::Sender<AudioRouting>,
    /// System token for portal communication (immutable, always-valid)
    pub system_token: Option<String>,
}

#[derive(Clone, Default)]
pub struct SequenceHeaders {
    pub video: Option<Bytes>,
    pub audio: Option<Bytes>,
    pub last_keyframe: Option<Bytes>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioChannelState {
    pub source_id: Uuid,
    pub muted: bool,
    /// Linear gain 0.0–1.0 applied in the programme audio mixer.
    pub volume: f32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioRouting {
    /// Which source provides audio when not mixing and not following video.
    pub active_audio_source: Option<Uuid>,
    /// Per-source mute / volume (used by Mix mode and single-source gain).
    pub channels: Vec<AudioChannelState>,
    /// If true, programme audio follows the Program video source (unless mixing).
    pub audio_follows_video: bool,
    /// If true, mix all unmuted live sources into programme audio (OBS-like).
    #[serde(default)]
    pub mix_live_sources: bool,
}

impl Default for AudioRouting {
    fn default() -> Self {
        Self {
            active_audio_source: None,
            channels: Vec::new(),
            audio_follows_video: true,
            mix_live_sources: false,
        }
    }
}

impl AudioRouting {
    pub fn ensure_channel(&mut self, source_id: Uuid) {
        if !self.channels.iter().any(|c| c.source_id == source_id) {
            self.channels.push(AudioChannelState {
                source_id,
                muted: false,
                volume: 1.0,
            });
        }
    }

    pub fn channel(&self, source_id: Uuid) -> Option<&AudioChannelState> {
        self.channels.iter().find(|c| c.source_id == source_id)
    }

    pub fn channel_mut(&mut self, source_id: Uuid) -> &mut AudioChannelState {
        self.ensure_channel(source_id);
        self.channels
            .iter_mut()
            .find(|c| c.source_id == source_id)
            .expect("channel just ensured")
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SourceMediaInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_bitrate_kbps: Option<u32>,
    pub audio_bitrate_kbps: Option<u32>,
    pub audio_sample_rate: Option<u32>,
    pub encoder: Option<String>,
}

impl AppState {
    pub async fn get_or_create_media_relay(
        &self,
        source_id: Uuid,
    ) -> broadcast::Sender<Bytes> {
        let mut relays = self.media_relays.write().await;
        relays
            .entry(source_id)
            .or_insert_with(|| broadcast::channel(4096).0)
            .clone()
    }

    pub async fn get_media_relay(
        &self,
        source_id: &Uuid,
    ) -> Option<broadcast::Sender<Bytes>> {
        let relays = self.media_relays.read().await;
        relays.get(source_id).cloned()
    }

    pub async fn remove_media_relay(&self, source_id: &Uuid) {
        let mut relays = self.media_relays.write().await;
        relays.remove(source_id);
    }
}
