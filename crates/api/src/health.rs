// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

//! Pipeline load monitoring: server CPU usage and whether each encoding stage
//! keeps up with real time, so the Studio can warn before the pipeline stalls.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::state::AppState;

/// How much media time a speed sample covers.
const METER_WINDOW: Duration = Duration::from_secs(5);
/// A running stage with no output for this long is reported as stalled.
const STALL_AFTER: Duration = Duration::from_secs(12);
const CPU_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

static NEXT_METER_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    /// Guest / WHIP decode + encode.
    Ingest,
    /// RTMP / SRT normalizer.
    Normalizer,
    /// Scene compositor.
    Compositor,
}

#[derive(Clone, Debug)]
struct StageStatus {
    kind: StageKind,
    speed: f32,
    updated: Instant,
    token: u64,
}

#[derive(Default)]
pub struct Health {
    cpu_percent: Mutex<Option<f32>>,
    stages: Mutex<HashMap<Uuid, StageStatus>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StageSnapshot {
    pub id: Uuid,
    pub kind: StageKind,
    /// Media seconds produced per wall-clock second (1.0 = real time).
    pub speed: f32,
}

impl Health {
    pub fn cpu_percent(&self) -> Option<f32> {
        *self.cpu_percent.lock().unwrap()
    }

    fn set_cpu_percent(&self, pct: f32) {
        *self.cpu_percent.lock().unwrap() = Some(pct);
    }

    fn report(&self, id: Uuid, kind: StageKind, speed: f32, token: u64, now: Instant) {
        self.stages.lock().unwrap().insert(
            id,
            StageStatus {
                kind,
                speed,
                updated: now,
                token,
            },
        );
    }

    fn clear(&self, id: Uuid, token: u64) {
        let mut stages = self.stages.lock().unwrap();
        if stages.get(&id).is_some_and(|s| s.token == token) {
            stages.remove(&id);
        }
    }

    pub fn stages(&self, now: Instant) -> Vec<StageSnapshot> {
        self.stages
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| StageSnapshot {
                id: *id,
                kind: s.kind,
                speed: if now.duration_since(s.updated) > STALL_AFTER {
                    0.0
                } else {
                    s.speed
                },
            })
            .collect()
    }
}

/// Timestamp (ms) of an FLV tag, from its 11-byte header.
pub fn flv_timestamp(tag_header: &[u8; 11]) -> u32 {
    ((tag_header[7] as u32) << 24)
        | ((tag_header[4] as u32) << 16)
        | ((tag_header[5] as u32) << 8)
        | (tag_header[6] as u32)
}

/// Measures how fast a stage's output media time advances against the wall
/// clock. An encoder that can't keep up produces less than one second of
/// media per second. Clears its entry when dropped.
pub struct RealtimeMeter {
    state: Arc<AppState>,
    id: Uuid,
    kind: StageKind,
    token: u64,
    window: Option<(Instant, u32)>,
}

impl RealtimeMeter {
    pub fn new(state: Arc<AppState>, id: Uuid, kind: StageKind) -> Self {
        Self {
            state,
            id,
            kind,
            token: NEXT_METER_TOKEN.fetch_add(1, Ordering::Relaxed),
            window: None,
        }
    }

    /// Feed every output tag; only video tags are measured.
    pub fn observe(&mut self, tag_header: &[u8; 11]) {
        if tag_header[0] != 9 {
            return;
        }
        let now = Instant::now();
        if let Some(speed) = window_speed(&mut self.window, flv_timestamp(tag_header), now) {
            self.state
                .health
                .report(self.id, self.kind, speed, self.token, now);
        }
    }
}

impl Drop for RealtimeMeter {
    fn drop(&mut self) {
        self.state.health.clear(self.id, self.token);
    }
}

/// Advance a measurement window; returns a speed sample when it closes.
fn window_speed(window: &mut Option<(Instant, u32)>, ts_ms: u32, now: Instant) -> Option<f32> {
    let Some((start, start_ts)) = *window else {
        *window = Some((now, ts_ms));
        return None;
    };
    let wall = now.duration_since(start);
    if wall < METER_WINDOW {
        return None;
    }
    *window = Some((now, ts_ms));
    let media_ms = ts_ms.wrapping_sub(start_ts);
    // Timestamp reset or jump: not a meaningful sample.
    if media_ms > (wall.as_millis() as u32).saturating_mul(4) {
        return None;
    }
    Some(media_ms as f32 / wall.as_millis() as f32)
}

#[derive(Clone, Copy, Debug)]
enum CpuSample {
    /// cgroup v2 with a CPU quota: usage in µs, measured against `cores`.
    Cgroup {
        usage_usec: u64,
        at: Instant,
        cores: f64,
    },
    /// Whole host from /proc/stat jiffies.
    Host { busy: u64, total: u64 },
}

/// Busy and total jiffies from the aggregate `cpu` line of /proc/stat.
fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest time is already in user)
    let total: u64 = fields.iter().take(8).sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

/// Cores allowed by a cgroup v2 `cpu.max` ("<quota> <period>" or "max <period>").
fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut parts = text.split_whitespace();
    let quota: f64 = parts.next()?.parse().ok()?;
    let period: f64 = parts.next()?.parse().ok()?;
    (period > 0.0).then(|| quota / period)
}

fn parse_cpu_stat_usage(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

fn read_cpu_sample() -> Option<CpuSample> {
    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
        .ok()
        .and_then(|t| parse_cpu_max(&t));
    if let Some(cores) = quota {
        if let Some(usage_usec) = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat")
            .ok()
            .and_then(|t| parse_cpu_stat_usage(&t))
        {
            return Some(CpuSample::Cgroup {
                usage_usec,
                at: Instant::now(),
                cores,
            });
        }
    }
    let (busy, total) = parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)?;
    Some(CpuSample::Host { busy, total })
}

fn cpu_percent_between(prev: CpuSample, cur: CpuSample) -> Option<f32> {
    let pct = match (prev, cur) {
        (
            CpuSample::Cgroup {
                usage_usec: u0,
                at: t0,
                ..
            },
            CpuSample::Cgroup {
                usage_usec: u1,
                at: t1,
                cores,
            },
        ) => {
            let wall_usec = t1.duration_since(t0).as_micros() as f64;
            if wall_usec <= 0.0 || cores <= 0.0 {
                return None;
            }
            u1.saturating_sub(u0) as f64 / (wall_usec * cores) * 100.0
        }
        (CpuSample::Host { busy: b0, total: t0 }, CpuSample::Host { busy: b1, total: t1 }) => {
            let total = t1.saturating_sub(t0);
            if total == 0 {
                return None;
            }
            b1.saturating_sub(b0) as f64 / total as f64 * 100.0
        }
        _ => return None,
    };
    Some(pct.clamp(0.0, 100.0) as f32)
}

/// Sample server CPU usage forever. Uses the container's cgroup quota when one
/// is set, otherwise the whole host.
pub async fn run_cpu_sampler(state: Arc<AppState>) {
    let mut prev = read_cpu_sample();
    if prev.is_none() {
        tracing::info!("health: CPU usage unavailable on this platform");
        return;
    }
    loop {
        tokio::time::sleep(CPU_SAMPLE_INTERVAL).await;
        let cur = read_cpu_sample();
        if let (Some(p), Some(c)) = (prev, cur) {
            if let Some(pct) = cpu_percent_between(p, c) {
                state.health.set_cpu_percent(pct);
            }
        }
        prev = cur;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_busy_excludes_idle_and_iowait() {
        let text = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_stat(text), Some((150, 1000)));
    }

    #[test]
    fn cpu_max_quota_and_unlimited() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(2.0));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
    }

    #[test]
    fn cpu_stat_usage() {
        let text = "usage_usec 123456\nuser_usec 1\nsystem_usec 2\n";
        assert_eq!(parse_cpu_stat_usage(text), Some(123456));
    }

    #[test]
    fn host_percent() {
        let a = CpuSample::Host { busy: 100, total: 1000 };
        let b = CpuSample::Host { busy: 400, total: 1400 };
        assert_eq!(cpu_percent_between(a, b), Some(75.0));
    }

    #[test]
    fn flv_timestamp_includes_extension_byte() {
        let mut h = [0u8; 11];
        h[4] = 0x01;
        h[5] = 0x02;
        h[6] = 0x03;
        h[7] = 0x04;
        assert_eq!(flv_timestamp(&h), 0x0401_0203);
    }

    #[test]
    fn window_reports_realtime_and_slow_speed() {
        let t0 = Instant::now();
        let mut w = None;
        assert_eq!(window_speed(&mut w, 1000, t0), None);
        assert_eq!(window_speed(&mut w, 3000, t0 + Duration::from_secs(2)), None);
        let s = window_speed(&mut w, 6000, t0 + Duration::from_secs(5)).unwrap();
        assert!((s - 1.0).abs() < 0.01);
        let s = window_speed(&mut w, 9000, t0 + Duration::from_secs(10)).unwrap();
        assert!((s - 0.6).abs() < 0.01);
    }

    #[test]
    fn window_ignores_timestamp_reset() {
        let t0 = Instant::now();
        let mut w = Some((t0, 50_000));
        assert_eq!(window_speed(&mut w, 100, t0 + Duration::from_secs(5)), None);
        let s = window_speed(&mut w, 5100, t0 + Duration::from_secs(10)).unwrap();
        assert!((s - 1.0).abs() < 0.01);
    }

    #[test]
    fn stale_stage_reads_as_stalled() {
        let h = Health::default();
        let id = Uuid::new_v4();
        let t0 = Instant::now();
        h.report(id, StageKind::Ingest, 1.0, 7, t0);
        assert_eq!(h.stages(t0)[0].speed, 1.0);
        assert_eq!(h.stages(t0 + Duration::from_secs(20))[0].speed, 0.0);
        h.clear(id, 8);
        assert_eq!(h.stages(t0).len(), 1);
        h.clear(id, 7);
        assert!(h.stages(t0).is_empty());
    }
}
