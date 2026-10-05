// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! H.264 encoder arguments shared by every ffmpeg stage that encodes video
//! (ingest normalizers, scene compositor, Channel HLS, transcoding egress, media
//! playback, browser sources), so the operator's encoder choice applies everywhere.
//!
//! A stage builds its ffmpeg command as:
//!   `input_args()` before the first `-i`, its own filter chain ending in
//!   `filter_tail()`, then `video_args()`.

use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::routes::output::{OutputConfig, VideoEncoder, X264Preset};

const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

fn vaapi_device() -> String {
    std::env::var("MUXSHED_VAAPI_DEVICE").unwrap_or_else(|_| DEFAULT_VAAPI_DEVICE.to_string())
}

/// Rate control for one encode.
pub struct Rate {
    pub kbps: u32,
    pub gop: u32,
    /// Output frame rate (`-r`); `None` keeps the input timing.
    pub fps: Option<u32>,
}

impl Rate {
    /// The output canvas rate: configured bitrate, 2s GOP, configured fps.
    pub fn canvas(cfg: &OutputConfig) -> Self {
        Self {
            kbps: cfg.video_bitrate_kbps,
            gop: cfg.fps * 2,
            fps: Some(cfg.fps),
        }
    }
}

/// Global args that must come before the first `-i` (hardware device setup).
pub fn input_args(cfg: &OutputConfig) -> Vec<String> {
    match cfg.video_encoder {
        VideoEncoder::Vaapi => vec!["-vaapi_device".into(), vaapi_device()],
        VideoEncoder::Software | VideoEncoder::Nvenc => vec![],
    }
}

/// Final filter(s) that hand frames to the encoder in the format it needs.
/// Always append this to the stage's video filter chain.
pub fn filter_tail(cfg: &OutputConfig) -> &'static str {
    match cfg.video_encoder {
        VideoEncoder::Vaapi => "format=nv12,hwupload",
        VideoEncoder::Software | VideoEncoder::Nvenc => "format=yuv420p",
    }
}

/// `-c:v` plus low-latency tuning, rate control, GOP and frame rate.
pub fn video_args(cfg: &OutputConfig, rate: &Rate) -> Vec<String> {
    build_video_args(cfg, rate, X264Preset::Veryfast)
}

/// As [`video_args`], with the operator's software preset for guest/WHIP ingest.
pub fn webrtc_ingest_video_args(cfg: &OutputConfig, rate: &Rate) -> Vec<String> {
    build_video_args(cfg, rate, cfg.webrtc_ingest_preset)
}

fn build_video_args(cfg: &OutputConfig, rate: &Rate, x264_preset: X264Preset) -> Vec<String> {
    let mut args: Vec<String> = match cfg.video_encoder {
        VideoEncoder::Software => vec![
            "-c:v", "libx264", "-preset", x264_preset.as_str(), "-tune", "zerolatency",
        ],
        VideoEncoder::Vaapi => vec!["-c:v", "h264_vaapi", "-rc_mode", "CBR"],
        VideoEncoder::Nvenc => vec![
            "-c:v", "h264_nvenc", "-preset", "p2", "-tune", "ll", "-zerolatency", "1", "-rc", "cbr",
        ],
    }
    .into_iter()
    .map(String::from)
    .collect();

    args.extend([
        "-bf".into(),
        "0".into(),
        "-b:v".into(),
        format!("{}k", rate.kbps),
        "-maxrate".into(),
        format!("{}k", rate.kbps),
        "-bufsize".into(),
        format!("{}k", rate.kbps * 2),
        "-g".into(),
        rate.gop.to_string(),
    ]);
    if let Some(fps) = rate.fps {
        args.extend(["-r".into(), fps.to_string()]);
    }
    args
}

/// Run a short test encode with the configured encoder, so a hardware choice the
/// host cannot support is rejected up front instead of failing every source.
pub async fn probe(cfg: &OutputConfig) -> Result<(), String> {
    if cfg.video_encoder == VideoEncoder::Software {
        return Ok(());
    }
    let mut args = vec!["-hide_banner".to_string(), "-loglevel".into(), "error".into()];
    args.extend(input_args(cfg));
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc2=s={}x{}:r={}:d=0.5", cfg.width, cfg.height, cfg.fps),
        "-vf".into(),
        filter_tail(cfg).into(),
    ]);
    args.extend(video_args(cfg, &Rate::canvas(cfg)));
    args.extend(["-f".into(), "null".into(), "-".into()]);

    let child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    let out = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .map_err(|_| "encoder test timed out".to_string())?
        .map_err(|e| format!("encoder test failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let detail = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error");
    Err(format!("{:?} encoder is not usable on this host: {}", cfg.video_encoder, detail.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enc: VideoEncoder) -> OutputConfig {
        OutputConfig {
            video_encoder: enc,
            ..OutputConfig::default()
        }
    }

    #[test]
    fn software_is_libx264_with_canvas_rate() {
        let c = cfg(VideoEncoder::Software);
        let a = video_args(&c, &Rate::canvas(&c)).join(" ");
        assert!(a.contains("-c:v libx264 -preset veryfast -tune zerolatency"));
        assert!(a.contains("-b:v 4500k -maxrate 4500k -bufsize 9000k -g 60 -r 30"));
        assert!(input_args(&c).is_empty());
        assert_eq!(filter_tail(&c), "format=yuv420p");
    }

    #[test]
    fn vaapi_uploads_to_gpu() {
        let c = cfg(VideoEncoder::Vaapi);
        assert_eq!(input_args(&c)[0], "-vaapi_device");
        assert_eq!(filter_tail(&c), "format=nv12,hwupload");
        assert!(video_args(&c, &Rate::canvas(&c)).join(" ").contains("h264_vaapi"));
    }

    #[test]
    fn webrtc_ingest_preset_only_changes_software_ingest() {
        let mut c = cfg(VideoEncoder::Software);
        c.webrtc_ingest_preset = X264Preset::Ultrafast;
        let r = Rate::canvas(&c);
        assert!(webrtc_ingest_video_args(&c, &r).join(" ").contains("-preset ultrafast"));
        assert!(video_args(&c, &r).join(" ").contains("-preset veryfast"));
        c.video_encoder = VideoEncoder::Vaapi;
        assert!(!webrtc_ingest_video_args(&c, &r).join(" ").contains("ultrafast"));
    }

    #[test]
    fn rate_without_fps_omits_r() {
        let c = cfg(VideoEncoder::Nvenc);
        let a = video_args(&c, &Rate { kbps: 2000, gop: 60, fps: None }).join(" ");
        assert!(a.contains("h264_nvenc"));
        assert!(!a.contains("-r "));
    }
}
