# Fork notes — halcycon/shed (guest UX)

Public fork of [muxshed/shed](https://github.com/muxshed/shed) with a guest “green room”
(local background blur, device controls, mic meter). Backend/pipeline changes are kept
minimal so upstream rebases stay straightforward.

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

- `web/src/routes/guest/[token]/+page.svelte` — green room UI + track management
- `web/src/lib/video-effects/` — background processor abstraction + MediaPipe blur
- `web/src/lib/audio/` — local mic level meter
- `web/static/mediapipe/` — WASM + selfie segmenter model (served locally)
- `crates/api/src/routes/guests.rs` — guest info includes Channel title/logo/accent
- `.github/workflows/docker.yml` — builds `guestux` branch and `*-guestux.*` tags
- `SPEC.md` — product requirements for this fork

Do **not** change source switching, ffmpeg/RTMP/SRT, scenes, auth, or DB schema for guest UX.

## Branding

Guest chrome uses **Studio → Channel** title / logo / accent (same settings as `/watch`).
There is no fork product name in the guest UI.

## Image tags

Prefer immutable tags:

```text
ghcr.io/halcycon/shed:1.8.6-guestux.1
```

Branch pushes also publish `ghcr.io/halcycon/shed:guestux` (mutable smoke tag).

Build via GitHub Actions on push to `guestux`, on tags matching `*-guestux.*`, or
`workflow_dispatch` with an optional tag input.

## Arcane deploy / rollback

1. Publish / pull `ghcr.io/halcycon/shed:1.8.6-guestux.N` (package must be public, or the
   host must be authenticated to GHCR).
2. In Arcane, change only the image:

   - from: `ghcr.io/muxshed/shed:1.8.6`
   - to:   `ghcr.io/halcycon/shed:1.8.6-guestux.N`

3. Keep `/config` and `/data` volumes unchanged.
4. Rollback: set the image back to `ghcr.io/muxshed/shed:1.8.6`.

## Rebase recipe

```sh
git fetch upstream
git checkout guestux
git rebase df367af   # or newer upstream tag/commit once verified
# resolve conflicts — usually only guest page / guests.rs / docker workflow
git push --force-with-lease origin guestux
```

After a successful rebase onto a newer upstream release, retag images as
`<upstream>-guestux.<n>` (e.g. `1.9.0-guestux.1`).

## Dependencies added (web)

| Package | Licence | Purpose |
|---------|---------|---------|
| `@mediapipe/tasks-vision` | Apache-2.0 | On-device person segmentation |

Model: MediaPipe `selfie_segmenter` (float16), vendored under `web/static/mediapipe/`.
Processing is entirely in the guest browser; frames are not uploaded to third parties.
