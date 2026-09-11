# Muxshed media pipeline (fork)

How media moves through this fork, and where encode is **required** vs **copy**.

Upstream advertises: **RTMP fan-out is forwarded (not re-encoded)**; the public watch page
(HLS) is the intentional transcoder. This fork restores that for egress.

```
Contribution
  WHIP VP8/Opus (browser guest) ──► ffmpeg normalize ──► H.264+AAC FLV relay
  WHIP H.264/Opus (OBS/HW)      ──► video copy + Opus→AAC ──► FLV relay*
  RTMP / SRT                    ──► source_normalizer ──► H.264+AAC FLV relay

Program bus (program.rs)
  Relay tags ──► remux only (timestamp / A/V route) ──► program_tx

Outputs from program_tx
  RTMP destinations ──► ffmpeg -c copy (default)   [no-transcode]
                     └── or transcode if OutputConfig.transcode_egress=true
  Channel HLS       ──► libx264+AAC (required for browser watch)
  Studio WHEP       ──► VP8+Opus (Preview + Program only; not the grid)

Scenes
  Multi-layer       ──► compositor encode (required)
  Single full-frame ──► identity forward (no encode)
```

\* H.264 WHIP remux assumes the publisher matches Studio output canvas
(`OutputConfig` width/height/fps). VP8 guests always normalize to canvas.

## Studio monitor latency (UI)

| Surface | Transport | Expectation |
|---------|-----------|-------------|
| Source grid | WS-FLV | ~2–5 s (labelled) |
| Preview / Program | WHEP → FLV fallback | Low latency when WHEP works |

Do **not** attach a WHEP encoder to every grid tile — that fights the no-transcode capacity model.

## Escape hatches

- `OutputConfig.transcode_egress = true` — letterbox/re-encode at RTMP egress (old behaviour, uses `veryfast` not `medium`).
- Hardware encode / public WHEP / NDI — deferred; not needed for producer snappiness under this model.

## Related

- `LATENCY-IMPROVEMENTS.md` — roadmap
- `FORK_NOTES.md` — deploy tags
