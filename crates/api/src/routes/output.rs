// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::ApiError;
use crate::state::AppState;

/// H.264 encoder used by every ffmpeg stage that encodes video.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum VideoEncoder {
    /// libx264 on the CPU.
    #[default]
    Software,
    /// Intel / AMD GPU via VA-API (`/dev/dri` must be passed into the container).
    Vaapi,
    /// Nvidia GPU via NVENC (needs the Nvidia container runtime).
    Nvenc,
}

/// libx264 speed preset. Faster presets cost less CPU for slightly lower
/// quality at the same bitrate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum X264Preset {
    #[default]
    Veryfast,
    Superfast,
    Ultrafast,
}

impl X264Preset {
    pub fn as_str(&self) -> &'static str {
        match self {
            X264Preset::Veryfast => "veryfast",
            X264Preset::Superfast => "superfast",
            X264Preset::Ultrafast => "ultrafast",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OutputConfig {
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// When true, RTMP egress re-encodes to the output canvas. Default false:
    /// remux (`-c copy`) to match Muxshed's no-transcode fan-out model.
    #[serde(default)]
    pub transcode_egress: bool,
    /// When true, Channel HLS re-encodes the programme. Default false: the bus
    /// is already canvas-sized H.264+AAC, so segment it with `-c copy`.
    #[serde(default)]
    pub transcode_hls: bool,
    #[serde(default)]
    pub video_encoder: VideoEncoder,
    /// Software-encoder preset for guest and WHIP ingest. Every guest runs its
    /// own canvas-sized encode, so this is the biggest per-guest CPU lever.
    #[serde(default)]
    pub webrtc_ingest_preset: X264Preset,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            video_bitrate_kbps: 4500,
            audio_bitrate_kbps: 160,
            width: 1920,
            height: 1080,
            fps: 30,
            transcode_egress: false,
            transcode_hls: false,
            video_encoder: VideoEncoder::Software,
            webrtc_ingest_preset: X264Preset::Veryfast,
        }
    }
}

impl OutputConfig {
    /// Load the persisted output config (defaults when unset or unreadable).
    pub async fn load(state: &AppState) -> Self {
        sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = 'output_config'")
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .and_then(|(json,)| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, utoipa::ToSchema)]
pub struct OutputStats {
    pub bytes_sent: u64,
    pub duration_secs: f64,
    pub source_bitrate_kbps: f64,
    pub output_bitrate_kbps: u32,
    pub dropped_frames: u64,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_fps: Option<f64>,
    pub source_encoder: Option<String>,
}

/// Get the encoder output config.
#[utoipa::path(
    get,
    path = "/api/v1/output/config",
    tag = "stream",
    responses((status = 200, description = "Output config", body = OutputConfig))
)]
pub async fn get_config(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OutputConfig>, ApiError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM settings WHERE key = 'output_config'")
            .fetch_optional(&state.db)
            .await?;

    let config = match row {
        Some((json,)) => serde_json::from_str(&json).unwrap_or_default(),
        None => OutputConfig::default(),
    };

    Ok(Json(config))
}

/// Set the encoder output config.
#[utoipa::path(
    put,
    path = "/api/v1/output/config",
    tag = "stream",
    request_body = OutputConfig,
    responses((status = 200, description = "Updated output config", body = OutputConfig))
)]
pub async fn set_config(
    State(state): State<Arc<AppState>>,
    Json(mut config): Json<OutputConfig>,
) -> Result<Json<OutputConfig>, ApiError> {
    // Clamp defensively so a bad value can't wedge the encoder.
    config.width = config.width.clamp(160, 3840);
    config.height = config.height.clamp(120, 2160);
    config.fps = config.fps.clamp(1, 60);
    config.video_bitrate_kbps = config.video_bitrate_kbps.clamp(500, 20000);
    config.audio_bitrate_kbps = config.audio_bitrate_kbps.clamp(32, 320);

    crate::encoder::probe(&config)
        .await
        .map_err(muxshed_common::MuxshedError::BadRequest)?;

    let json = serde_json::to_string(&config)
        .map_err(|e| muxshed_common::MuxshedError::Internal(e.to_string()))?;

    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES ('output_config', ?)")
        .bind(&json)
        .execute(&state.db)
        .await?;

    // Apply immediately if a stream is live: restart the egress and the watch-page
    // encoder with the new config (a brief destination reconnect, like failover).
    let live = !matches!(
        state.pipeline.state().await,
        muxshed_common::PipelineState::Idle | muxshed_common::PipelineState::Stopping
    );
    if live {
        state.egress.set_output_config(Some(config.clone())).await;
        let program_src = *state.program_source.borrow();
        if let Some(src) = program_src {
            crate::egress_restart_for(&state, src).await;
        }
        crate::routes::stream::ensure_channel_hls(&state).await;
    }

    Ok(Json(config))
}

/// Get live output/egress stats.
#[utoipa::path(
    get,
    path = "/api/v1/output/stats",
    tag = "stream",
    responses((status = 200, description = "Output stats", body = OutputStats))
)]
pub async fn get_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OutputStats>, ApiError> {
    let mut stats = state.egress.stats().await;

    // Get configured output bitrate
    let out_config: OutputConfig =
        sqlx::query_as::<_, (String,)>("SELECT value FROM settings WHERE key = 'output_config'")
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .and_then(|(json,)| serde_json::from_str(&json).ok())
            .unwrap_or_default();
    stats.output_bitrate_kbps = out_config.video_bitrate_kbps + out_config.audio_bitrate_kbps;

    // Get current program source's media info
    let program_id = *state.program_source.borrow();
    if let Some(id) = program_id {
        let infos = state.source_media_info.read().await;
        if let Some(info) = infos.get(&id) {
            stats.source_width = info.width;
            stats.source_height = info.height;
            stats.source_fps = info.fps;
            stats.source_encoder = info.encoder.clone();
        }
    }

    Ok(Json(stats))
}
