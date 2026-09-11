// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Capture a short live-source clip and suggest jive-inspired spoken-word DSP.
//!
//! This is **analyse → suggest → apply**, not the full jive-vocals file pipeline
//! and not jive-room PipeWire live DSP. Suggestions map to conservative ffmpeg
//! filters on the programme mixer bus (lip-sync safe).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::state::{AppState, AudioDspFilters};
use bytes::Bytes;

const CAPTURE_SECS: u64 = 12;

#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioAnalyseResult {
    pub source_id: Uuid,
    pub duration_secs: f32,
    pub lufs_i: Option<f32>,
    pub true_peak_db: Option<f32>,
    pub rms_level_db: Option<f32>,
    pub suggestion: AudioDspFilters,
    pub notes: Vec<String>,
}

/// Record ~12s of a live source relay to a temp FLV, analyse, return suggestions.
pub async fn analyse_source(
    state: Arc<AppState>,
    source_id: Uuid,
) -> Result<AudioAnalyseResult, String> {
    let relay = state
        .get_media_relay(&source_id)
        .await
        .ok_or_else(|| "source has no media relay (not live?)".to_string())?;

    let data_dir = state.config.read().await.data_dir.clone();
    let dir = data_dir.join("audio-analyse");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create analyse dir: {e}"))?;
    let path = dir.join(format!("{source_id}.flv"));
    let _ = tokio::fs::remove_file(&path).await;

    capture_relay_flv(relay, state.clone(), source_id, &path, CAPTURE_SECS).await?;

    let metrics = run_ffmpeg_metrics(&path).await?;
    let (suggestion, notes) = suggest_from_metrics(&metrics);

    let _ = tokio::fs::remove_file(&path).await;

    Ok(AudioAnalyseResult {
        source_id,
        duration_secs: CAPTURE_SECS as f32,
        lufs_i: metrics.lufs_i,
        true_peak_db: metrics.true_peak_db,
        rms_level_db: metrics.rms_level_db,
        suggestion,
        notes,
    })
}

async fn capture_relay_flv(
    relay: broadcast::Sender<Bytes>,
    state: Arc<AppState>,
    source_id: Uuid,
    path: &PathBuf,
    secs: u64,
) -> Result<(), String> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("create capture file: {e}"))?;

    let header = crate::rtmp::flv::flv_header();
    file.write_all(&header)
        .await
        .map_err(|e| format!("write flv header: {e}"))?;

    {
        let headers = state.sequence_headers.read().await;
        if let Some(h) = headers.get(&source_id) {
            if let Some(v) = &h.video {
                let _ = file.write_all(v).await;
            }
            if let Some(a) = &h.audio {
                let _ = file.write_all(a).await;
            }
            if let Some(k) = &h.last_keyframe {
                let _ = file.write_all(k).await;
            }
        }
    }

    let mut rx = relay.subscribe();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut wrote_audio = false;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(data) => {
                        if data.first() == Some(&8) {
                            wrote_audio = true;
                        }
                        if file.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(remaining) => break,
        }
    }
    file.flush().await.map_err(|e| format!("flush capture: {e}"))?;

    if !wrote_audio {
        return Err("no audio packets captured — is the source sending audio?".into());
    }
    Ok(())
}

#[derive(Debug, Default)]
struct Metrics {
    lufs_i: Option<f32>,
    true_peak_db: Option<f32>,
    rms_level_db: Option<f32>,
}

async fn run_ffmpeg_metrics(path: &PathBuf) -> Result<Metrics, String> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostats",
            "-i",
            path.to_str().unwrap_or(""),
            "-vn",
            "-af",
            "aformat=sample_rates=48000:channel_layouts=mono,ebur128=peak=true,astats=metadata=1:reset=1",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg analyse: {e}"))?;

    let stderr = child.stderr.take().ok_or("no stderr")?;
    let mut reader = BufReader::new(stderr).lines();
    let mut metrics = Metrics::default();
    let mut log = String::new();

    while let Ok(Some(line)) = reader.next_line().await {
        log.push_str(&line);
        log.push('\n');
        // ebur128: "I:         -23.0 LUFS"
        if let Some(v) = parse_after(&line, "I:") {
            if line.contains("LUFS") {
                metrics.lufs_i = Some(v);
            }
        }
        if let Some(v) = parse_after(&line, "Peak:") {
            if line.contains("dBFS") {
                metrics.true_peak_db = Some(v);
            }
        }
        // astats: "RMS level dB:    -20.12"
        if let Some(v) = parse_after(&line, "RMS level dB:") {
            metrics.rms_level_db = Some(v);
        }
    }

    let _ = child.wait().await;

    if metrics.lufs_i.is_none() && metrics.rms_level_db.is_none() {
        tracing::warn!("audio analyse: no metrics parsed; ffmpeg log tail:\n{}",
            log.chars().rev().take(800).collect::<String>().chars().rev().collect::<String>());
    }

    Ok(metrics)
}

fn parse_after(line: &str, key: &str) -> Option<f32> {
    let idx = line.find(key)?;
    let rest = line[idx + key.len()..].trim();
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '.' || *c == '+')
        .collect();
    num.parse().ok()
}

fn suggest_from_metrics(m: &Metrics) -> (AudioDspFilters, Vec<String>) {
    let mut notes = Vec::new();
    let mut filters = AudioDspFilters {
        enabled: true,
        highpass_hz: 80.0,
        denoise: false,
        gate: false,
        compress: false,
        notes: Vec::new(),
    };
    notes.push("High-pass ~80 Hz to cut rumble (jive-style spoken-word default).".into());

    if let Some(i) = m.lufs_i {
        notes.push(format!("Integrated loudness ≈ {i:.1} LUFS over the sample."));
        if i < -32.0 {
            notes.push("Very quiet — raise the strip volume or ask the guest to speak up.".into());
            filters.compress = true;
            notes.push("Gentle compressor suggested to lift quieter speech.".into());
        } else if i < -24.0 {
            filters.compress = true;
            notes.push("A bit quiet for programme — light compression suggested.".into());
        } else if i > -12.0 {
            notes.push("Hot level — watch peaks; compressor suggested to even dynamics.".into());
            filters.compress = true;
        }
    }

    if let Some(rms) = m.rms_level_db {
        notes.push(format!("Overall RMS ≈ {rms:.1} dBFS."));
        // Very low RMS with speech expected → noisy/quiet room; enable light denoise+gate.
        if rms < -45.0 {
            filters.denoise = true;
            filters.gate = true;
            notes.push("Low RMS / likely room noise — light denoise + gate suggested.".into());
        } else if rms < -35.0 {
            filters.gate = true;
            notes.push("Moderate noise floor — gate suggested between phrases.".into());
        }
    }

    if let Some(tp) = m.true_peak_db {
        notes.push(format!("True peak ≈ {tp:.1} dBFS."));
        if tp > -1.0 {
            notes.push("Peaks near clipping — reduce guest input gain if possible.".into());
            filters.compress = true;
        }
    }

    if !filters.compress && !filters.denoise && !filters.gate {
        notes.push("Levels look reasonable — high-pass alone is a safe default.".into());
    }

    filters.notes = notes.clone();
    (filters, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_lufs_gets_compress() {
        let m = Metrics {
            lufs_i: Some(-36.0),
            true_peak_db: Some(-8.0),
            rms_level_db: Some(-40.0),
        };
        let (f, _) = suggest_from_metrics(&m);
        assert!(f.compress);
        assert!(f.highpass_hz > 0.0);
    }

    #[test]
    fn noisy_rms_gets_denoise_gate() {
        let m = Metrics {
            lufs_i: Some(-20.0),
            true_peak_db: Some(-5.0),
            rms_level_db: Some(-50.0),
        };
        let (f, _) = suggest_from_metrics(&m);
        assert!(f.denoise);
        assert!(f.gate);
    }

    #[test]
    fn filter_chain_builds() {
        let f = AudioDspFilters {
            enabled: true,
            highpass_hz: 80.0,
            denoise: true,
            gate: false,
            compress: true,
            notes: vec![],
        };
        let chain = f.to_ffmpeg_chain().unwrap();
        assert!(chain.contains("highpass"));
        assert!(chain.contains("afftdn"));
        assert!(chain.contains("acompressor"));
    }
}
