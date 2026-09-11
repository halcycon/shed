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
- `crates/api/src/routes/guests.rs` — guest info includes Channel title/logo/accent
- `SPEC.md` — guest UX requirements

### Studio / Channel / latency

- `crates/api/src/channel_hls.rs` — deterministic HLS bootstrap (seq headers + keyframe gate)
- `crates/api/src/routes/stream.rs` — prime HLS with video+audio headers for the effective programme
- `crates/api/src/program.rs` — `resolve_program_audio_source` (scenes without AAC → first layer)
- `crates/api/src/scene_compositor.rs` — video-only scene output (no `anullsrc` silence)
- `crates/api/src/program_whep.rs` + `routes/whep_program.rs` — **Program + source WHEP** hubs
- `web/src/components/WhepMonitor.svelte` + `whep-player.ts` — WHEP with WS-FLV fallback
- `web/src/components/ProgramMonitor.svelte` — thin Program wrapper around `WhepMonitor`
- Studio Preview + popout Preview use source WHEP; source **grid thumbnails stay WS-FLV**
- `web/src/routes/(app)/channel/+page.svelte` — remove stale GStreamer wording
- `web/src/components/VideoPreview.svelte` — optional `monitorAudio` + real analyser levels
- Studio / popout Program: local “Monitor Audio” toggle (headphones recommended)
- Fake `Math.random()` mixer meters replaced with Program-preview analyser levels
- `LATENCY-IMPROVEMENTS.md` — roadmap (PR 1–2 WHEP shipped; later phases not started)

### Packaging

- `.github/workflows/docker.yml` — builds `guestux` branch and `*-guestux.*` tags
- `docker/Dockerfile` — runtime includes `wget` so common orchestrator healthchecks work
  (Arcane’s default `wget`-based probe failed on the stock image)

## Root causes (stretch fixes)

1. **Slow Channel HLS** — late subscribers to `program_tx` often missed codec config / joined
   mid-GOP. ChannelHls now primes like egress (seq headers + cached keyframe) and drops
   packets until the first live keyframe; re-gates after mid-stream AVC config / lag.
2. **Scene silence** — compositor mapped `anullsrc`, and Audio Follows Video selected the
   scene id, so programme audio was synthetic silence. Scenes are video-only; the program
   router picks layer (or independent) audio instead.
3. **GStreamer copy** — Channel UI still mentioned a GStreamer build; HLS has always been ffmpeg.

## Program + Preview WHEP (latency PR 1–2)

Studio monitors can use **WHEP** (auth required):

| Feed | Endpoint | Encoder input |
|------|----------|----------------|
| Program | `POST /api/v1/program/whep` | `program_tx` FLV |
| Preview / one source | `POST /api/v1/sources/{id}/whep` | that source’s `media_relays` FLV |

- Shared ffmpeg per active feed: FLV → VP8 + Opus RTP → send-only WebRTC peers
- UI tries WHEP first; on failure falls back to WS-FLV
- **Only Preview + Program** get WHEP (one encoder per feed). Source grid stays FLV
- Monitor Audio remains local-only / muted by default
- Later items (HW encode, NDI, public WHEP) stay in `LATENCY-IMPROVEMENTS.md`

### Latency check (manual)

Display a ms stopwatch to a guest camera; photograph guest → Preview / Program (WHEP)
vs prior WS-FLV. Record before/after when convenient.

## Branding

Guest chrome uses **Studio → Channel** title / logo / accent (same settings as `/watch`).
There is no fork product name in the guest UI.

## Image tags

Prefer immutable tags:

```text
ghcr.io/halcycon/shed:1.8.6-guestux.8
```

Branch pushes also publish `ghcr.io/halcycon/shed:guestux` (mutable smoke tag).

## Arcane deploy / rollback

1. Pull `ghcr.io/halcycon/shed:1.8.6-guestux.8` (or newer).
2. In Arcane, set image to that tag (volumes unchanged).
3. Rollback: `ghcr.io/muxshed/shed:1.8.6`.

## Upstream

Guest green room: https://github.com/muxshed/shed/pull/25 · https://github.com/muxshed/shed/issues/26  
HLS / scene-audio / monitor fixes are also clean enough to propose upstream separately.

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
```

### Acceptance (live studio)

- Enable Public Channel while already ON AIR → playlist within ~6s
- Scene on Program → guest layer audio audible (no silence)
- Program “Monitor Audio” toggles local speaker only; mixer bars track real levels
- Guest blur / WHIP / Opus unchanged from prior guestux builds
