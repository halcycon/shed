// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Normalizes RTMP source input to a consistent output canvas resolution.
//! Each source gets its own FFmpeg process: raw FLV in -> normalized FLV out.
//! The normalized output feeds the public media relay so the program router
//! only ever switches between streams of identical resolution/codec/framerate.
//!
//! Audio-only RTMP sources (soundboard / bed) skip video and remux/encode AAC.

use bytes::Bytes;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::routes::output::OutputConfig;
use crate::state::AppState;
use muxshed_common::SourceKind;

/// Start the normalizer for an RTMP source.
/// Returns a sender for raw FLV data (what the RTMP ingest writes to).
/// The normalizer reads from this, pipes through FFmpeg, and writes
/// normalized FLV into the source's public media relay.
pub async fn start_normalizer(
    state: Arc<AppState>,
    source_id: Uuid,
) -> Result<broadcast::Sender<Bytes>, String> {
    let cfg = load_output_config(&state).await;
    let audio_only = load_audio_only(&state, source_id).await;

    let (raw_tx, _) = broadcast::channel::<Bytes>(4096);
    let raw_tx_clone = raw_tx.clone();

    let public_tx = state.get_or_create_media_relay(source_id).await;

    let ba = format!("{}k", cfg.audio_bitrate_kbps);

    let args: Vec<String> = if audio_only {
        tracing::info!("starting audio-only normalizer for {}", source_id);
        vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "warning".into(),
            "-f".into(),
            "flv".into(),
            "-i".into(),
            "pipe:0".into(),
            "-vn".into(),
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
        ]
    } else {
        let vf = format!(
            "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:black",
            cfg.width, cfg.height, cfg.width, cfg.height
        );
        let bv = format!("{}k", cfg.video_bitrate_kbps);
        let maxrate = format!("{}k", cfg.video_bitrate_kbps);
        let bufsize = format!("{}k", cfg.video_bitrate_kbps * 2);
        let gop = format!("{}", cfg.fps * 2);
        let fps = format!("{}", cfg.fps);

        tracing::info!(
            "starting source normalizer for {} ({}x{}@{}fps)",
            source_id, cfg.width, cfg.height, cfg.fps
        );

        vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "warning".into(),
            "-f".into(),
            "flv".into(),
            "-i".into(),
            "pipe:0".into(),
            "-vf".into(),
            vf,
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            "-tune".into(),
            "zerolatency".into(),
            "-bf".into(),
            "0".into(),
            "-b:v".into(),
            bv,
            "-maxrate".into(),
            maxrate,
            "-bufsize".into(),
            bufsize,
            "-g".into(),
            gop,
            "-r".into(),
            fps,
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            ba,
            "-ar".into(),
            "48000".into(),
            "-f".into(),
            "flv".into(),
            "-flvflags".into(),
            "no_duration_filesize".into(),
            "pipe:1".into(),
        ]
    };

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start normalizer ffmpeg: {}", e))?;

    let stdin = child.stdin.take().ok_or("no stdin")?;
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
                    Ok(_) => tracing::debug!("normalizer [{}]: {}", sid, line.trim()),
                    Err(_) => break,
                }
            }
        });
    }

    {
        let mut normalizers = state.source_normalizers.write().await;
        if let Some(mut old) = normalizers.remove(&source_id) {
            let _ = old.kill().await;
        }
        normalizers.insert(source_id, child);
    }

    let mut raw_rx = raw_tx.subscribe();
    let write_audio_header = audio_only;
    tokio::spawn(async move {
        let mut stdin = stdin;
        let header = if write_audio_header {
            crate::rtmp::flv::flv_header_audio_only()
        } else {
            crate::rtmp::flv::flv_header()
        };
        if stdin.write_all(&header).await.is_err() {
            return;
        }

        loop {
            match raw_rx.recv().await {
                Ok(data) => {
                    if stdin.write_all(&data).await.is_err() {
                        tracing::debug!("normalizer stdin closed for source");
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("normalizer raw rx lagged {} for source", n);
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let state_clone = state.clone();
    tokio::spawn(async move {
        read_normalized_output(source_id, stdout, public_tx, state_clone).await;
    });

    Ok(raw_tx_clone)
}

async fn load_audio_only(state: &AppState, source_id: Uuid) -> bool {
    let row = sqlx::query_as::<_, (String,)>("SELECT kind FROM sources WHERE id = ?")
        .bind(source_id.to_string())
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    row.and_then(|(json,)| serde_json::from_str::<SourceKind>(&json).ok())
        .map(|k| k.is_audio_only())
        .unwrap_or(false)
}

async fn read_normalized_output(
    source_id: Uuid,
    mut stdout: tokio::process::ChildStdout,
    public_tx: broadcast::Sender<Bytes>,
    state: Arc<AppState>,
) {
    let mut header = [0u8; 13];
    if stdout.read_exact(&mut header).await.is_err() {
        tracing::warn!("normalizer: failed to read FLV header for {}", source_id);
        return;
    }

    let mut frame_count: u64 = 0;

    loop {
        let mut tag_header = [0u8; 11];
        if stdout.read_exact(&mut tag_header).await.is_err() {
            break;
        }

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
                let entry = headers.entry(source_id).or_default();
                entry.video = Some(tag.clone());
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
                let entry = headers.entry(source_id).or_default();
                entry.audio = Some(tag.clone());
            }
        }

        let _ = public_tx.send(tag);
        frame_count += 1;

        if frame_count == 1 {
            tracing::info!("normalizer: streaming normalized output for {}", source_id);
        }
    }

    tracing::info!("normalizer: ended for {} after {} frames", source_id, frame_count);
    let mut normalizers = state.source_normalizers.write().await;
    normalizers.remove(&source_id);
}

pub async fn stop_normalizer(state: &AppState, source_id: &Uuid) {
    let mut normalizers = state.source_normalizers.write().await;
    if let Some(mut child) = normalizers.remove(source_id) {
        let _ = child.kill().await;
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
