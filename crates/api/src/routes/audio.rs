// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::{AppState, AudioRouting};
use muxshed_common::MuxshedError;

async fn persist_routing(state: &AppState, routing: &AudioRouting) {
    if let Ok(json) = serde_json::to_string(routing) {
        let _ = sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES ('audio_routing', ?)")
            .bind(&json)
            .execute(&state.db)
            .await;
    }
}

pub async fn get_routing(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AudioRouting>, ApiError> {
    let mut routing = state.audio_routing.borrow().clone();
    let live: Vec<Uuid> = state
        .source_states
        .read()
        .await
        .iter()
        .filter(|(_, s)| **s == muxshed_common::SourceState::Live)
        .map(|(id, _)| *id)
        .collect();
    for id in live {
        routing.ensure_channel(id);
    }
    Ok(Json(routing))
}

pub async fn set_routing(
    State(state): State<Arc<AppState>>,
    Json(mut routing): Json<AudioRouting>,
) -> Result<Json<AudioRouting>, ApiError> {
    for ch in routing.channels.iter_mut() {
        ch.volume = ch.volume.clamp(0.0, 2.0);
    }
    let _ = state.audio_routing.send(routing.clone());
    persist_routing(&state, &routing).await;
    Ok(Json(routing))
}

#[derive(Deserialize)]
pub struct SetAudioSourceRequest {
    pub source_id: Option<Uuid>,
}

pub async fn set_audio_source(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetAudioSourceRequest>,
) -> Result<StatusCode, ApiError> {
    if let Some(id) = body.source_id {
        let states = state.source_states.read().await;
        if states.get(&id) != Some(&muxshed_common::SourceState::Live) {
            return Err(MuxshedError::BadRequest("source is not live".to_string()).into());
        }
    }

    state.audio_routing.send_modify(|routing| {
        routing.mix_live_sources = false;
        routing.active_audio_source = body.source_id;
        routing.audio_follows_video = body.source_id.is_none();
        if let Some(id) = body.source_id {
            routing.ensure_channel(id);
        }
    });

    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(StatusCode::OK)
}

pub async fn toggle_follows_video(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    state.audio_routing.send_modify(|routing| {
        routing.mix_live_sources = false;
        routing.audio_follows_video = !routing.audio_follows_video;
        if routing.audio_follows_video {
            routing.active_audio_source = None;
        }
    });

    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(StatusCode::OK)
}

pub async fn toggle_mix(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AudioRouting>, ApiError> {
    state.audio_routing.send_modify(|routing| {
        routing.mix_live_sources = !routing.mix_live_sources;
        if routing.mix_live_sources {
            routing.audio_follows_video = false;
        }
    });
    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(Json(routing))
}

pub async fn mute_source(
    State(state): State<Arc<AppState>>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: Uuid = source_id
        .parse()
        .map_err(|_| MuxshedError::BadRequest("invalid uuid".to_string()))?;
    state.audio_routing.send_modify(|routing| {
        routing.channel_mut(id).muted = true;
    });

    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(StatusCode::OK)
}

pub async fn unmute_source(
    State(state): State<Arc<AppState>>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: Uuid = source_id
        .parse()
        .map_err(|_| MuxshedError::BadRequest("invalid uuid".to_string()))?;
    state.audio_routing.send_modify(|routing| {
        routing.channel_mut(id).muted = false;
    });

    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct SetVolumeRequest {
    pub volume: f32,
}

pub async fn set_volume(
    State(state): State<Arc<AppState>>,
    Path(source_id): Path<String>,
    Json(body): Json<SetVolumeRequest>,
) -> Result<StatusCode, ApiError> {
    let id: Uuid = source_id
        .parse()
        .map_err(|_| MuxshedError::BadRequest("invalid uuid".to_string()))?;
    let vol = body.volume.clamp(0.0, 2.0);
    state.audio_routing.send_modify(|routing| {
        routing.channel_mut(id).volume = vol;
    });

    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(StatusCode::OK)
}

/// Capture ~12s of a live source and suggest jive-inspired spoken-word DSP.
pub async fn analyse_source(
    State(state): State<Arc<AppState>>,
    Path(source_id): Path<String>,
) -> Result<Json<crate::audio_analyse::AudioAnalyseResult>, ApiError> {
    let id: Uuid = source_id
        .parse()
        .map_err(|_| MuxshedError::BadRequest("invalid uuid".to_string()))?;
    let states = state.source_states.read().await;
    if states.get(&id) != Some(&muxshed_common::SourceState::Live) {
        return Err(MuxshedError::BadRequest("source is not live".to_string()).into());
    }
    drop(states);

    let result = crate::audio_analyse::analyse_source(state.clone(), id)
        .await
        .map_err(|e| MuxshedError::BadRequest(e))?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct SetFiltersRequest {
    pub filters: crate::state::AudioDspFilters,
}

/// Apply (or clear) DSP filters on a mixer strip.
pub async fn set_filters(
    State(state): State<Arc<AppState>>,
    Path(source_id): Path<String>,
    Json(body): Json<SetFiltersRequest>,
) -> Result<Json<AudioRouting>, ApiError> {
    let id: Uuid = source_id
        .parse()
        .map_err(|_| MuxshedError::BadRequest("invalid uuid".to_string()))?;
    state.audio_routing.send_modify(|routing| {
        routing.channel_mut(id).filters = body.filters.clone();
    });
    let routing = state.audio_routing.borrow().clone();
    persist_routing(&state, &routing).await;
    Ok(Json(routing))
}
