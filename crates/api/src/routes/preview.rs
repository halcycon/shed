// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::Response;
use std::sync::Arc;

use crate::rtmp::flv;
use crate::state::AppState;

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    ws.on_upgrade(move |socket| handle_preview(socket, state, id))
}

async fn handle_preview(mut socket: WebSocket, state: Arc<AppState>, id: String) {
    let source_id: uuid::Uuid = match id.parse() {
        Ok(id) => id,
        Err(_) => {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };

    let relay = state.get_media_relay(&source_id).await;
    let Some(tx) = relay else {
        tracing::debug!("no active media relay for source {}", source_id);
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let mut rx = tx.subscribe();
    tracing::info!("preview client connected for source {}", source_id);

    // Composited scenes carry no audio; players that are told to expect audio
    // wait for it forever and never show the picture.
    let header = if is_composited_scene(&state, source_id).await {
        flv::flv_header_video_only()
    } else {
        flv::flv_header()
    };
    if socket
        .send(Message::Binary(header.to_vec().into()))
        .await
        .is_err()
    {
        return;
    }

    // Send cached sequence headers + last keyframe so clients can render immediately
    let cached = state.sequence_headers.read().await.get(&source_id).cloned();
    if let Some(seq) = cached {
        if let Some(ref video) = seq.video {
            if socket.send(Message::Binary(video.to_vec().into())).await.is_err() {
                return;
            }
        }
        if let Some(ref audio) = seq.audio {
            if socket.send(Message::Binary(audio.to_vec().into())).await.is_err() {
                return;
            }
        }
        if let Some(ref keyframe) = seq.last_keyframe {
            if socket.send(Message::Binary(keyframe.to_vec().into())).await.is_err() {
                return;
            }
        }
    }

    let mut frame_count: u64 = 0;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(data) => {
                        frame_count += 1;
                        if frame_count <= 5 || frame_count.is_multiple_of(300) {
                            tracing::debug!("preview frame #{} ({} bytes) for {}", frame_count, data.len(), source_id);
                        }
                        if socket.send(Message::Binary(data.to_vec().into())).await.is_err() {
                            tracing::debug!("preview send failed at frame {}", frame_count);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("preview lagged {} messages for {}", n, source_id);
                        continue;
                    }
                    Err(e) => {
                        tracing::debug!("preview channel closed: {:?}", e);
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

/// A scene whose output comes from the compositor (identity scenes forward
/// their layer's audio and video unchanged).
async fn is_composited_scene(state: &AppState, id: uuid::Uuid) -> bool {
    if state.scene_forward_cancels.read().await.contains_key(&id) {
        return false;
    }
    sqlx::query_as::<_, (i64,)>("SELECT 1 FROM scenes WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .is_some()
}
