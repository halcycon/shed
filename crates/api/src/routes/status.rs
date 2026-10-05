// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

use crate::error::ApiError;
use crate::state::AppState;
use muxshed_common::PipelineState;

#[derive(Serialize, utoipa::ToSchema)]
pub struct StatusResponse {
    pub pipeline: PipelineState,
}

/// Current pipeline status.
#[utoipa::path(
    get,
    path = "/api/v1/status",
    tag = "status",
    responses((status = 200, description = "Pipeline status", body = StatusResponse))
)]
pub async fn get_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<StatusResponse>, ApiError> {
    let pipeline = state.pipeline.state().await;
    Ok(Json(StatusResponse { pipeline }))
}

/// Below this many media seconds per wall second a stage is falling behind.
const BEHIND_SPEED: f32 = 0.9;
const CPU_WARN_PERCENT: f32 = 90.0;

#[derive(Serialize, utoipa::ToSchema)]
pub struct StageLoad {
    pub id: uuid::Uuid,
    pub name: String,
    pub kind: crate::health::StageKind,
    /// Media seconds produced per wall-clock second (1.0 = real time).
    pub speed: f32,
    pub behind: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LoadResponse {
    /// Server CPU usage (container quota when limited); null if unavailable.
    pub cpu_percent: Option<f32>,
    pub cpu_cores: usize,
    pub overloaded: bool,
    pub stages: Vec<StageLoad>,
}

/// Server CPU usage and whether each encoding stage keeps up with real time.
#[utoipa::path(
    get,
    path = "/api/v1/status/load",
    tag = "status",
    responses((status = 200, description = "Pipeline load", body = LoadResponse))
)]
pub async fn get_load(State(state): State<Arc<AppState>>) -> Result<Json<LoadResponse>, ApiError> {
    let cpu_percent = state.health.cpu_percent();
    let mut stages = Vec::new();
    for s in state.health.stages(std::time::Instant::now()) {
        let id = s.id.to_string();
        let name = sqlx::query_scalar::<_, String>(
            "SELECT name FROM sources WHERE id = ? UNION ALL SELECT name FROM scenes WHERE id = ? LIMIT 1",
        )
        .bind(&id)
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(id);
        stages.push(StageLoad {
            id: s.id,
            name,
            kind: s.kind,
            speed: s.speed,
            behind: s.speed < BEHIND_SPEED,
        });
    }
    stages.sort_by(|a, b| a.speed.total_cmp(&b.speed));
    let overloaded =
        cpu_percent.is_some_and(|c| c >= CPU_WARN_PERCENT) || stages.iter().any(|s| s.behind);
    Ok(Json(LoadResponse {
        cpu_percent,
        cpu_cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
        overloaded,
        stages,
    }))
}
