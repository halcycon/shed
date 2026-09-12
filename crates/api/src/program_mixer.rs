// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Programme audio mixer — OBS-like multi-source mix on the Muxshed bus.
//!
//! Takes Program video (`-c:v copy`) plus one or more live AAC legs, applies
//! per-source volume, `amix`es them, and publishes H.264+AAC FLV onto
//! `program_tx` so RTMP/HLS stay in lip-sync with guest video.

use bytes::Bytes;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::watch;
use uuid::Uuid;

use crate::routes::output::OutputConfig;
use crate::state::{AppState, AudioDspFilters};

const FLV_TAG_AUDIO: u8 = 8;
const FLV_TAG_VIDEO: u8 = 9;

/// One unmuted audio contribution into the programme mix.
#[derive(Debug, Clone)]
pub struct MixLeg {
    pub source_id: Uuid,
    pub volume: f32,
    pub filters: AudioDspFilters,
    pub duck_others: bool,
    /// Residual gain on other legs while this ducker is hot (0.05–1.0).
    pub duck_level: f32,
}

/// Run until `cancel` is true. Writes mixed programme FLV tags to `program_tx`.
pub async fn run_program_mixer(
    state: Arc<AppState>,
    video_id: Uuid,
    legs: Vec<MixLeg>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), String> {
    if legs.is_empty() {
        return Err("program mixer: no audio legs".into());
    }

    let cfg = load_output_config(&state).await;
    let video_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("mixer video listen: {e}"))?;
    let video_port = video_listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();

    let mut audio_listeners = Vec::new();
    let mut audio_ports = Vec::new();
    for _ in &legs {
        let l = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("mixer audio listen: {e}"))?;
        audio_ports.push(l.local_addr().map_err(|e| e.to_string())?.port());
        audio_listeners.push(l);
    }

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-fflags".into(),
        "nobuffer+genpts".into(),
        "-flags".into(),
        "low_delay".into(),
        "-analyzeduration".into(),
        "500000".into(),
        "-probesize".into(),
        "500000".into(),
        "-f".into(),
        "flv".into(),
        "-i".into(),
        format!("tcp://127.0.0.1:{video_port}"),
    ];

    for p in &audio_ports {
        args.extend([
            "-analyzeduration".into(),
            "500000".into(),
            "-probesize".into(),
            "500000".into(),
            "-f".into(),
            "flv".into(),
            "-i".into(),
            format!("tcp://127.0.0.1:{p}"),
        ]);
    }

    // Build per-leg DSP + volume, then amix. Input 0 = video; 1..N = audio legs.
    let filter = build_mix_filter_graph(&legs);

    let ba = format!("{}k", cfg.audio_bitrate_kbps);
    args.extend([
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "[aout]".into(),
        "-c:v".into(),
        "copy".into(),
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
        "program mixer: video={} + {} audio leg(s)",
        video_id,
        legs.len()
    );

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("start program mixer: {e}"))?;

    let stdout = child.stdout.take().ok_or("mixer: no stdout")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                    Ok(0) => break,
                    Ok(_) => tracing::debug!("mixer: {}", line.trim()),
                    Err(_) => break,
                }
            }
        });
    }

    // Feed video + audio sockets.
    {
        let st = state.clone();
        tokio::spawn(async move {
            feed_source_flv(video_listener, video_id, st).await;
        });
    }
    for (listener, leg) in audio_listeners.into_iter().zip(legs.iter()) {
        let sid = leg.source_id;
        let st = state.clone();
        tokio::spawn(async move {
            feed_source_flv(listener, sid, st).await;
        });
    }

    let program_tx = state.program_tx.clone();
    let read_state = state.clone();
    let mut reader = tokio::spawn(async move {
        publish_mixer_output(stdout, program_tx, read_state, video_id).await;
    });

    loop {
        tokio::select! {
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    break;
                }
            }
            _ = &mut reader => break,
        }
    }

    let _ = child.kill().await;
    reader.abort();
    Ok(())
}

async fn feed_source_flv(listener: TcpListener, source_id: Uuid, state: Arc<AppState>) {
    let Ok((mut sock, _)) = listener.accept().await else {
        return;
    };
    let _ = sock.set_nodelay(true);
    if sock
        .write_all(&crate::rtmp::flv::flv_header())
        .await
        .is_err()
    {
        return;
    }
    let Some(relay) = state.get_media_relay(&source_id).await else {
        return;
    };
    let mut rx = relay.subscribe();
    {
        let headers = state.sequence_headers.read().await;
        if let Some(h) = headers.get(&source_id) {
            if let Some(v) = &h.video {
                let _ = sock.write_all(v).await;
            }
            if let Some(a) = &h.audio {
                let _ = sock.write_all(a).await;
            }
            if let Some(k) = &h.last_keyframe {
                let _ = sock.write_all(k).await;
            }
        }
    }
    loop {
        match rx.recv().await {
            Ok(data) => {
                if sock.write_all(&data).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

async fn publish_mixer_output(
    mut stdout: tokio::process::ChildStdout,
    program_tx: tokio::sync::broadcast::Sender<Bytes>,
    state: Arc<AppState>,
    video_id: Uuid,
) {
    let mut header = [0u8; 13];
    if stdout.read_exact(&mut header).await.is_err() {
        tracing::warn!("program mixer: failed to read FLV header");
        return;
    }

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

        // Cache mixer's AAC config on the video source entry so HLS priming can find it.
        if tag_type == FLV_TAG_AUDIO && !data.is_empty() && data.get(1) == Some(&0) {
            let mut headers = state.sequence_headers.write().await;
            let entry = headers.entry(video_id).or_default();
            entry.audio = Some(tag.clone());
        } else if tag_type == FLV_TAG_VIDEO && !data.is_empty() {
            let avc = if data.len() > 1 { data[1] } else { 255 };
            let frame_type = (data[0] >> 4) & 0x0F;
            if avc == 0 {
                let mut headers = state.sequence_headers.write().await;
                let entry = headers.entry(video_id).or_default();
                entry.video = Some(tag.clone());
            } else if frame_type == 1 {
                let mut headers = state.sequence_headers.write().await;
                if let Some(entry) = headers.get_mut(&video_id) {
                    entry.last_keyframe = Some(tag.clone());
                }
            }
        }

        let _ = program_tx.send(tag);
    }
    tracing::info!("program mixer: output ended");
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

fn build_mix_filter_graph(legs: &[MixLeg]) -> String {
    let ducker_idxs: Vec<usize> = legs
        .iter()
        .enumerate()
        .filter(|(_, l)| l.duck_others)
        .map(|(i, _)| i)
        .collect();
    let other_idxs: Vec<usize> = legs
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.duck_others)
        .map(|(i, _)| i)
        .collect();
    let use_duck = !ducker_idxs.is_empty() && !other_idxs.is_empty() && legs.len() > 1;

    // Per-leg preprocess: DSP + volume + aformat → labeled pads
    let mut parts = Vec::new();
    for (i, leg) in legs.iter().enumerate() {
        let vol = leg.volume.clamp(0.0, 2.0);
        let idx = i + 1;
        let out = if !use_duck && legs.len() == 1 {
            "aout".to_string()
        } else {
            format!("a{i}")
        };
        let mut chain = String::new();
        if let Some(dsp) = leg.filters.to_ffmpeg_chain() {
            chain.push_str(&dsp);
            chain.push(',');
        }
        chain.push_str(&format!(
            "volume={vol:.3},aformat=sample_fmts=fltp:channel_layouts=stereo:sample_rates=48000"
        ));
        parts.push(format!("[{idx}:a]{chain}[{out}]"));
    }

    if !use_duck {
        if legs.len() == 1 {
            return parts[0].clone();
        }
        let labels: String = (0..legs.len()).map(|i| format!("[a{i}]")).collect();
        parts.push(format!(
            "{}amix=inputs={}:duration=longest:dropout_transition=0:normalize=0[aout]",
            labels,
            legs.len()
        ));
        return parts.join(";");
    }

    // Mix of residual levels across duckers (how quiet others get when any ducker is hot).
    let residual = ducker_idxs
        .iter()
        .map(|&i| legs[i].duck_level.clamp(0.05, 1.0))
        .fold(1.0_f32, f32::min);
    // sidechaincompress mix: 1 = full duck toward compressed; residual≈0.25 → mix≈0.75
    let sc_mix = (1.0 - residual).clamp(0.0, 1.0);
    // ~-30 dBFS as linear amplitude
    let threshold = 0.0316;

    // Split each ducker into mix pad + sidechain contribution.
    let mut sc_labels = Vec::new();
    for &i in &ducker_idxs {
        parts.push(format!("[a{i}]asplit=2[dm{i}][dsc{i}]"));
        sc_labels.push(format!("[dsc{i}]"));
    }

    if sc_labels.len() == 1 {
        let i = ducker_idxs[0];
        parts.push(format!("[dsc{i}]anull[sc]"));
    } else {
        let n = sc_labels.len();
        parts.push(format!(
            "{}amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0[sc]",
            sc_labels.join("")
        ));
    }

    // Split combined SC once per other leg (sidechaincompress consumes the SC input).
    let m = other_idxs.len();
    if m == 1 {
        parts.push("[sc]anull[sc0]".into());
    } else {
        let outs: String = (0..m).map(|j| format!("[sc{j}]")).collect();
        parts.push(format!("[sc]asplit={m}{outs}"));
    }

    let mut final_labels = Vec::new();
    for &i in &ducker_idxs {
        final_labels.push(format!("[dm{i}]"));
    }
    for (j, &i) in other_idxs.iter().enumerate() {
        parts.push(format!(
            "[a{i}][sc{j}]sidechaincompress=threshold={threshold}:ratio=6:attack=40:release=250:level_sc=1:mix={sc_mix:.3}[do{i}]"
        ));
        final_labels.push(format!("[do{i}]"));
    }

    let n = final_labels.len();
    parts.push(format!(
        "{}amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0[aout]",
        final_labels.join("")
    ));
    parts.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AudioDspFilters;

    fn leg(duck: bool) -> MixLeg {
        MixLeg {
            source_id: Uuid::nil(),
            volume: 1.0,
            filters: AudioDspFilters::default(),
            duck_others: duck,
            duck_level: 0.25,
        }
    }

    #[test]
    fn plain_mix_has_no_sidechain() {
        let g = build_mix_filter_graph(&[leg(false), leg(false)]);
        assert!(g.contains("amix"));
        assert!(!g.contains("sidechaincompress"));
    }

    #[test]
    fn duck_graph_uses_sidechain() {
        let g = build_mix_filter_graph(&[leg(true), leg(false)]);
        assert!(g.contains("sidechaincompress"));
        assert!(g.contains("asplit"));
    }
}
