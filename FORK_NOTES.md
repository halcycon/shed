# Fork notes — halcycon/shed (guest UX + studio fixes)

Public fork of [muxshed/shed](https://github.com/muxshed/shed). Backend/pipeline changes
stay minimal so upstream rebases remain straightforward.

## Pin point

| Item | Value |
|------|--------|
| Upstream | `muxshed/shed` |
| Base commit | `df367af08f1800eff5c5cb8c8676c10d40e8ef65` (Cargo / GHCR `1.8.6`) |
| Feature branch | `guestux` |
| Images | `ghcr.io/halcycon/shed:<tag>` |

Confirm the running host still matches this pin before rebasing:

```sh
podman image inspect ghcr.io/muxshed/shed:1.8.6 \
  --format '{{index .Labels "org.opencontainers.image.revision"}}'
```

## Custom surface area

### Guest green room

- `web/src/routes/guest/[token]/+page.svelte` — green room UI + track management
- `web/src/lib/video-effects/` — background processor + MediaPipe blur
- `web/src/lib/audio/mic-meter.ts` — guest mic meter
- `web/static/mediapipe/` — WASM + selfie segmenter (served locally)
- `crates/api/src/routes/guests.rs` — guest info includes Channel title/logo/accent; a guest
  keeps one source across rejoins (scene layers / program selection survive), deleted with the guest
- Guest page asks for 1280x720@30 and auto-reconnects with backoff (2s → 30s) when the
  connection drops or the browser comes back online
- `SPEC.md` — guest UX requirements

### Studio / Channel / latency

- `PIPELINE.md` — encode vs copy stage map (upstream no-transcode vision)
- Programme **audio mixer** (AFV / Independent / Mix): mute, volume, multi-source `amix` on bus
- `crates/api/src/program_mixer.rs` — ffmpeg video-copy + AAC mix → `program_tx`; **Duck** via `sidechaincompress`
- `crates/api/src/audio_analyse.rs` — capture ~12s → ebur128/astats → suggest DSP
- Mixer strips: **Analyse / Apply / Clear** (jive-inspired HP/denoise/gate/compress on bus)
- **Audio-only RTMP** (`SourceKind::Rtmp.audio_only`) — AAC-only normalizer, no black video; Mix + Duck for soundboards
- `crates/api/src/egress.rs` — **RTMP `-c copy` by default**; `OutputConfig.transcode_egress` escape hatch
- `crates/api/src/webrtc_ingest.rs` — WHIP/guest ingest always normalizes onto the canvas (VP8 or H.264 in, Opus→AAC)
- `crates/api/src/scene_compositor.rs` — low-delay flags; identity scene skips compositor encode; bilinear layer scaling, YUV `lut` opacity
- `crates/api/src/channel_hls.rs` — deterministic HLS bootstrap (seq headers + keyframe gate); **`-c copy` by default**, `OutputConfig.transcode_hls` escape hatch
- `crates/api/src/encoder.rs` — `OutputConfig.video_encoder` (`software` / `vaapi` / `nvenc`) shared by every encoding stage; GPU choices are test-encoded on save.
  `OutputConfig.webrtc_ingest_preset` (`veryfast` / `superfast` / `ultrafast`) trades guest/WHIP ingest quality for CPU on the software encoder
- `crates/api/src/media_player.rs` — single-file playback normalized onto the canvas (copy egress/HLS rely on a uniform bus)
- `crates/api/src/webrtc_ingest.rs` — 15s grace on ICE `disconnected` before tearing down;
  teardown only acts on the current peer (a rejoin can't be killed by the old one); PLI once a
  second only until the source is live, then only on unrepaired RTP loss (≤ every 2s) instead of
  forcing a keyframe every 2s
- `crates/api/src/health.rs` + `GET /api/v1/status/load` — server CPU (cgroup quota when set,
  else host) and per-stage realtime speed (ingest / normalizer / compositor); Studio shows a CPU
  pill and a SERVER OVERLOADED banner after two bad polls (CPU ≥ 90% or a stage < 0.9× real time)
- `crates/api/src/routes/stream.rs` — prime HLS with video+audio headers for the effective programme
- `crates/api/src/program.rs` — `resolve_program_audio_source` (scenes without AAC → first layer)
- `crates/api/src/program_whep.rs` + `routes/whep_program.rs` — WHEP API kept (unused by Studio UI)
- Studio Preview/Program/popouts: **tuned WS-FLV** (`VideoPreview` `profile="monitor"`)
- Source grid: safer FLV buffer + `~2–5s` cue; monitors show `Monitor FLV`
- `LATENCY-IMPROVEMENTS.md` — WHEP Studio monitors withdrawn; no-transcode egress kept

### Packaging

- `.github/workflows/docker.yml` — builds `guestux` branch and `*-guestux.*` tags
- `docker/Dockerfile` — runtime includes `wget` so common orchestrator healthchecks work
  (Arcane’s default `wget`-based probe failed on the stock image), plus VA-API drivers
  for the optional GPU encoder (pass `/dev/dri` into the container to use it;
  `MUXSHED_VAAPI_DEVICE` overrides `/dev/dri/renderD128`)

## Root causes (stretch fixes)

1. **Slow Channel HLS** — late subscribers to `program_tx` often missed codec config / joined
   mid-GOP. ChannelHls now primes like egress (seq headers + cached keyframe) and drops
   packets until the first live keyframe; re-gates after mid-stream AVC config / lag.
2. **Scene silence** — compositor mapped `anullsrc`, and Audio Follows Video selected the
   scene id, so programme audio was synthetic silence. Scenes are video-only; the program
   router picks layer (or independent) audio instead.
3. **GStreamer copy** — Channel UI still mentioned a GStreamer build; HLS has always been ffmpeg.
4. **Egress always transcoded** — contradicted upstream “RTMP forwarded not re-encoded”; default
   is remux again. Going live used to spin `libx264 -preset medium` per destination and starve
   Program WHEP.
5. **Studio WHEP slower than FLV** — monitor path re-encoded to VP8; under live+HLS load it
   lagged and keyframe-stalled. Studio UI reverted to chased WS-FLV (guestux.10).
6. **Watch ON AIR but black** — Channel HLS dropped live packets after priming a cached
   keyframe until the *next* IDR, which could leave ffmpeg encoding black while the
   playlist still updated. Fixed to forward immediately after a successful prime
   (guestux.11).
7. **Stall with guests in a scene** — the identity-scene path held a `sequence_headers` read
   lock while taking the write lock (tokio `RwLock` deadlock), freezing everything that touched
   headers. Readers now clone headers before awaiting.
8. **Reconnects left stale subscribers** — a reconnecting source gets a fresh media relay, but
   the program router (passthrough and Mix), failover monitor and scene compositors kept the old
   one. They now listen for `SourceState` Live/Disconnected and re-subscribe or restart
   (`scene_compositor::run_scene_supervisor` for the on-air scene).

## Studio monitors (guestux.10)

Preview / Program use **WS-FLV** with aggressive mpegts latency chasing — no WebRTC re-encode.
Source switches reuse the same component (no full remount / WHEP renegotiate).

WHEP endpoints remain in the API for experiments; Studio does not open them.

### Latency check (manual)

Grid should feel more buffered than Preview/Program. Going live + opening `/watch` must not
make Program slower than the grid.

## Branding

Guest chrome uses **Studio → Channel** title / logo / accent (same settings as `/watch`).
There is no fork product name in the guest UI.

## Image tags

Prefer immutable tags:

```text
ghcr.io/halcycon/shed:1.8.6-guestux.14
```

Branch pushes also publish `ghcr.io/halcycon/shed:guestux` (mutable smoke tag).

## Arcane deploy / rollback

1. Pull `ghcr.io/halcycon/shed:1.8.6-guestux.14` (or newer).
2. In Arcane, set image to that tag (volumes unchanged).
3. Rollback: `ghcr.io/muxshed/shed:1.8.6`.

## Upstream

Guest green room: https://github.com/muxshed/shed/pull/25 · https://github.com/muxshed/shed/issues/26  
HLS / scene-audio / monitor fixes are also clean enough to propose upstream separately.
Egress copy restoration is a strong candidate for upstream.

## Rebase recipe

```sh
git fetch upstream
git checkout guestux
git rebase df367af   # or newer upstream tag/commit once verified
git push --force-with-lease origin guestux
```

## Dependencies added (web)

| Package | Licence | Purpose |
|---------|---------|---------|
| `@mediapipe/tasks-vision` | Apache-2.0 | On-device person segmentation |

Model: MediaPipe `selfie_segmenter` (float16), vendored under `web/static/mediapipe/`.

## Build / test

```sh
cd web && npm ci && npm run check && npm run build
cargo test -p muxshed-api channel_hls --lib
cargo test -p muxshed-api scene_compositor --lib
cargo test -p muxshed-api audio_analyse --lib
cargo test -p muxshed-api program_mixer --lib
cargo test -p muxshed-common --lib
```

### Acceptance (live studio)

- Enable Public Channel while already ON AIR → playlist within ~6s
- Scene on Program → guest layer audio audible (no silence)
- Program “Monitor Audio” toggles local speaker only; mixer bars track real levels
- Guest blur / WHIP / Opus unchanged from prior guestux builds
- Go live with RTMP dest → logs show `egress: remux copy`
- Source grid shows `~2–5s`; Preview/Program show `Monitor FLV` and switch quickly
- While live + `/watch` open, Program must not feel slower than the grid
- Audio strip **Analyse** (~12s) → notes + suggestion → **Apply** inserts bus DSP; **Clear DSP** removes it
- Mix + DSP keeps lips on the Muxshed bus (not offboard)
- Create RTMP **Audio only** source → publish AAC-only → Live; strip in Audio mixer, not Program video
- Mix + **Duck** on soundboard → play FX → other legs dip, then recover
