// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::state::AppState;

/// POST /api/v1/program/whep — WHEP playback of the Program bus (auth required).
pub async fn create(
    State(state): State<Arc<AppState>>,
    offer: String,
) -> Response {
    if offer.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty SDP offer" })),
        )
            .into_response();
    }

    match state
        .program_whep
        .create_session(state.clone(), offer)
        .await
    {
        Ok((session_id, answer)) => Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/sdp")
            .header(
                header::LOCATION,
                format!("/api/v1/program/whep/{}", session_id),
            )
            .body(axum::body::Body::from(answer))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(e) => {
            tracing::warn!("program WHEP create failed: {}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    }
}

/// DELETE /api/v1/program/whep/{session}
pub async fn teardown(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
) -> StatusCode {
    let Ok(id) = Uuid::parse_str(&session) else {
        return StatusCode::BAD_REQUEST;
    };
    if state.program_whep.delete_session(id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
