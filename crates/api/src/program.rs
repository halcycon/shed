// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

use crate::program_mixer::MixLeg;
use crate::rtmp::flv;
use crate::state::AppState;
use bytes::Bytes;
use muxshed_common::SourceState;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

const FLV_TAG_AUDIO: u8 = 8;
const FLV_TAG_VIDEO: u8 = 9;

fn tag_type(data: &[u8]) -> Option<u8> {
    if data.is_empty() {
        return None;
    }
    Some(data[0])
}

fn is_video_keyframe(data: &[u8]) -> bool {
    if data.len() < 13 || data[0] != FLV_TAG_VIDEO {
        return false;
    }
    let frame_type = data[11] & 0xF0;
    let is_seq_header = data[12] == 0x00;
    frame_type == 0x10 && !is_seq_header
}

fn is_sequence_header(data: &[u8]) -> bool {
    if data.len() < 13 {
        return false;
    }
    let tt = data[0];
    if tt == FLV_TAG_VIDEO {
        return data[12] == 0x00;
    }
    if tt == FLV_TAG_AUDIO && data.len() > 12 {
        return data[12] == 0x00;
    }
    false
}

/// Remap a tag's timestamp relative to a base, outputting a continuous timestamp.
fn remap_tag(data: &[u8], first_ts: &mut Option<u32>, base_output: u32, output_ts: &mut u32) -> Bytes {
    if let Some(src_ts) = flv::read_tag_timestamp(data) {
        let base = first_ts.get_or_insert(src_ts);
        let elapsed = src_ts.wrapping_sub(*base);
        let new_ts = base_output.wrapping_add(elapsed);
        *output_ts = new_ts;
        flv::rewrite_tag_timestamp(data, new_ts)
    } else {
        Bytes::copy_from_slice(data)
    }
}

/// Resolve which source should provide programme audio for the given video source.
///
/// Independent routing wins when set. Otherwise audio follows video — but if the
/// video source has no AAC sequence header (e.g. a video-only scene compositor),
/// fall back to the first scene layer so "Audio Follows Video" is not silence.
pub async fn resolve_program_audio_source(
    state: &AppState,
    video_source_id: Uuid,
    routing: &crate::state::AudioRouting,
) -> Uuid {
    if !routing.audio_follows_video {
        if let Some(id) = routing.active_audio_source {
            return id;
        }
    }

    {
        let headers = state.sequence_headers.read().await;
        if let Some(h) = headers.get(&video_source_id) {
            if h.audio.is_some() {
                return video_source_id;
            }
        }
    }

    // Scene layers are ordered by z_index; use the bottom-most live layer as default AFV audio.
    if let Ok(Some(layer_source)) = sqlx::query_as::<_, (String,)>(
        "SELECT source_id FROM scene_layers WHERE scene_id = ? ORDER BY z_index ASC LIMIT 1",
    )
    .bind(video_source_id.to_string())
    .fetch_optional(&state.db)
    .await
    {
        if let Ok(id) = Uuid::parse_str(&layer_source.0) {
            tracing::info!(
                "program audio: scene {} has no audio — using layer source {}",
                video_source_id,
                id
            );
            return id;
        }
    }

    video_source_id
}

/// Build the audio legs that should contribute to programme audio.
pub async fn resolve_audio_legs(
    state: &AppState,
    video_source_id: Uuid,
    routing: &crate::state::AudioRouting,
) -> Vec<MixLeg> {
    let headers = state.sequence_headers.read().await;
    let live: Vec<Uuid> = state
        .source_states
        .read()
        .await
        .iter()
        .filter(|(_, s)| **s == SourceState::Live)
        .map(|(id, _)| *id)
        .collect();

    let mut legs = Vec::new();

    if routing.mix_live_sources {
        for id in live {
            let has_audio = headers.get(&id).and_then(|h| h.audio.as_ref()).is_some();
            if !has_audio {
                continue;
            }
            let ch = routing.channel(id);
            let muted = ch.map(|c| c.muted).unwrap_or(false);
            let volume = ch.map(|c| c.volume).unwrap_or(1.0);
            if muted || volume < 0.001 {
                continue;
            }
            legs.push(MixLeg {
                source_id: id,
                volume: volume.clamp(0.0, 2.0),
            });
        }
    } else {
        let audio_id = resolve_program_audio_source(state, video_source_id, routing).await;
        let has_audio = headers
            .get(&audio_id)
            .and_then(|h| h.audio.as_ref())
            .is_some();
        if has_audio {
            let ch = routing.channel(audio_id);
            let muted = ch.map(|c| c.muted).unwrap_or(false);
            let volume = ch.map(|c| c.volume).unwrap_or(1.0);
            if !muted && volume >= 0.001 {
                legs.push(MixLeg {
                    source_id: audio_id,
                    volume: volume.clamp(0.0, 2.0),
                });
            }
        }
    }

    legs
}

fn needs_ffmpeg_mixer(routing: &crate::state::AudioRouting, legs: &[MixLeg]) -> bool {
    if legs.is_empty() {
        return false;
    }
    if routing.mix_live_sources {
        return true;
    }
    if legs.len() > 1 {
        return true;
    }
    legs.iter().any(|l| (l.volume - 1.0).abs() > 0.02)
}

/// Runs the program router: takes video from program_source, audio from audio routing.
/// Guarantees a gapless output stream by keeping the old source running until
/// the new source produces a keyframe.
pub async fn run_program_router(state: Arc<AppState>) {
    let mut source_rx = state.program_source.subscribe();
    let mut audio_routing_rx = state.audio_routing.subscribe();
    let mut output_ts: u32 = 0;

    loop {
        let current_source_id = loop {
            let current = *source_rx.borrow_and_update();
            if let Some(id) = current {
                break id;
            }
            if source_rx.changed().await.is_err() {
                return;
            }
        };

        let audio_routing = audio_routing_rx.borrow_and_update().clone();
        let legs = resolve_audio_legs(&state, current_source_id, &audio_routing).await;

        if needs_ffmpeg_mixer(&audio_routing, &legs) {
            tracing::info!(
                "program router: mixer mode video={} legs={}",
                current_source_id,
                legs.len()
            );
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let st = state.clone();
            let vid = current_source_id;
            let legs_c = legs.clone();
            let join = tokio::spawn(async move {
                if let Err(e) =
                    crate::program_mixer::run_program_mixer(st, vid, legs_c, cancel_rx).await
                {
                    tracing::warn!("program mixer stopped: {e}");
                }
            });
            tokio::select! {
                result = source_rx.changed() => {
                    if result.is_err() {
                        let _ = cancel_tx.send(true);
                        let _ = join.await;
                        return;
                    }
                }
                result = audio_routing_rx.changed() => {
                    if result.is_err() {
                        let _ = cancel_tx.send(true);
                        let _ = join.await;
                        return;
                    }
                }
            }
            let _ = cancel_tx.send(true);
            let _ = join.await;
            continue;
        }

        let audio_source_id = legs
            .first()
            .map(|l| l.source_id)
            .unwrap_or(current_source_id);
        let drop_audio = legs.is_empty();
        let same_source = current_source_id == audio_source_id && !drop_audio;

        tracing::info!(
            "program router: passthrough video={} audio={} (same={} silent={})",
            current_source_id, audio_source_id, same_source, drop_audio
        );

        if !drop_audio {
            send_sequence_headers(&state, &current_source_id, &audio_source_id, output_ts).await;
        } else if let Some(seq) = state.sequence_headers.read().await.get(&current_source_id) {
            if let Some(ref video) = seq.video {
                let remapped = flv::rewrite_tag_timestamp(video, output_ts);
                let _ = state.program_tx.send(remapped);
            }
        }

        let video_relay = state.get_media_relay(&current_source_id).await;
        let audio_relay = if same_source || drop_audio {
            None
        } else {
            state.get_media_relay(&audio_source_id).await
        };

        let Some(video_tx) = video_relay else {
            tracing::warn!("program router: no relay for video source {}", current_source_id);
            if source_rx.changed().await.is_err() { return; }
            continue;
        };

        let mut video_rx = video_tx.subscribe();
        let mut audio_rx = audio_relay.map(|tx| tx.subscribe());

        let mut forwarded: u64 = 0;
        let mut got_keyframe = false;
        let mut first_video_ts: Option<u32> = None;
        let mut first_audio_ts: Option<u32> = None;
        let base_output_ts = output_ts;

        // Pending switch state: when a source change is requested, we subscribe
        // to the new source but keep forwarding the old one until we get a keyframe.
        let mut pending_switch: Option<PendingSwitch> = None;

        loop {
            tokio::select! {
                // Source changed
                result = source_rx.changed() => {
                    if result.is_err() { return; }
                    let new_id = *source_rx.borrow_and_update();
                    if let Some(new_source_id) = new_id {
                        if new_source_id == current_source_id {
                            continue;
                        }
                        // Subscribe to new source but keep forwarding old one
                        if let Some(new_relay) = state.get_media_relay(&new_source_id).await {
                            tracing::info!(
                                "program router: preparing switch {} -> {} (keeping old until keyframe)",
                                current_source_id, new_source_id
                            );
                            pending_switch = Some(PendingSwitch {
                                source_id: new_source_id,
                                rx: new_relay.subscribe(),
                                got_keyframe: false,
                            });
                        } else {
                            tracing::warn!("program router: no relay for new source {}, waiting", new_source_id);
                            pending_switch = Some(PendingSwitch {
                                source_id: new_source_id,
                                rx: broadcast::channel(1).0.subscribe(),
                                got_keyframe: false,
                            });
                        }
                    }
                }
                // Audio routing changed
                result = audio_routing_rx.changed() => {
                    if result.is_err() { return; }
                    // Re-enter outer loop to reconfigure audio
                    tracing::info!("program router: audio routing change");
                    break;
                }
                // New source data (when switch is pending)
                msg = async {
                    match &mut pending_switch {
                        Some(ps) => ps.rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Ok(data) => {
                            if let Some(ref mut ps) = pending_switch {
                                let tt = tag_type(&data);
                                // Skip sequence headers, we'll send our own
                                if is_sequence_header(&data) {
                                    continue;
                                }
                                if tt == Some(FLV_TAG_VIDEO) && is_video_keyframe(&data) {
                                    ps.got_keyframe = true;
                                    // Send sequence headers for new source
                                    let new_audio_routing = audio_routing_rx.borrow().clone();
                                    let new_audio_id = resolve_program_audio_source(
                                        &state,
                                        ps.source_id,
                                        &new_audio_routing,
                                    )
                                    .await;
                                    send_sequence_headers(&state, &ps.source_id, &new_audio_id, output_ts).await;
                                    // Forward the keyframe
                                    let remapped = remap_tag(&data, &mut Some(flv::read_tag_timestamp(&data).unwrap_or(0)), output_ts, &mut output_ts);
                                    let _ = state.program_tx.send(remapped);
                                    tracing::info!(
                                        "program router: switched to {} after {} frames from old source",
                                        ps.source_id, forwarded
                                    );
                                    // Break to outer loop which will re-enter with new source
                                    break;
                                }
                                // Drop non-keyframe video from new source while waiting
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => {
                            // New source relay closed, abort switch
                            pending_switch = None;
                        }
                    }
                }
                // Current source data (always forward)
                msg = video_rx.recv() => {
                    match msg {
                        Ok(data) => {
                            let tt = tag_type(&data);

                            // Wait for first keyframe on initial connect
                            if !got_keyframe && tt == Some(FLV_TAG_VIDEO) {
                                if is_video_keyframe(&data) {
                                    got_keyframe = true;
                                } else if !is_sequence_header(&data) {
                                    continue;
                                }
                            }

                            if !got_keyframe && tt == Some(FLV_TAG_AUDIO) && !same_source {
                                continue;
                            }

                            // Mute: drop audio tags from the video source.
                            if drop_audio && tt == Some(FLV_TAG_AUDIO) {
                                continue;
                            }

                            if same_source || tt == Some(FLV_TAG_VIDEO) || is_sequence_header(&data) {
                                let remapped = remap_tag(&data, &mut first_video_ts, base_output_ts, &mut output_ts);
                                let _ = state.program_tx.send(remapped);
                                forwarded += 1;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                // Separate audio source data
                msg = async {
                    match &mut audio_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Ok(data) => {
                            if tag_type(&data) == Some(FLV_TAG_AUDIO) {
                                let remapped = remap_tag(&data, &mut first_audio_ts, base_output_ts, &mut output_ts);
                                let _ = state.program_tx.send(remapped);
                                forwarded += 1;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => {
                            audio_rx = None;
                        }
                    }
                }
            }

            if forwarded == 1 || forwarded == 10 || forwarded.is_multiple_of(1000) {
                tracing::debug!("program router: forwarded {} packets, output_ts={}", forwarded, output_ts);
            }
        }
    }
}

struct PendingSwitch {
    source_id: Uuid,
    rx: broadcast::Receiver<Bytes>,
    #[allow(dead_code)]
    got_keyframe: bool,
}

async fn send_sequence_headers(
    state: &AppState,
    video_source_id: &Uuid,
    audio_source_id: &Uuid,
    ts: u32,
) {
    let headers = state.sequence_headers.read().await;

    if let Some(seq) = headers.get(video_source_id) {
        if let Some(ref video) = seq.video {
            let remapped = flv::rewrite_tag_timestamp(video, ts);
            let _ = state.program_tx.send(remapped);
        }
    }

    let audio_seq = headers.get(audio_source_id).or_else(|| headers.get(video_source_id));
    if let Some(seq) = audio_seq {
        if let Some(ref audio) = seq.audio {
            let remapped = flv::rewrite_tag_timestamp(audio, ts);
            let _ = state.program_tx.send(remapped);
        }
    }
}
