# Muxshed Fork Roadmap — Low-Latency Monitoring, Media Pipeline Optimisation and NDI

## Project context

We are maintaining a small fork of:

- Upstream: `muxshed/shed`
- Current deployment: Docker via Arcane
- Muxshed runs on a cloud-hosted Linux server
- Public Studio UI is behind HTTPS / Cloudflare Tunnel
- Remote guests currently contribute using WebRTC/WHIP
- TURN is already configured and working
- RTMP and SRT ingest exist upstream
- Public Channel uses HLS
- Current media engine is ffmpeg-based

Current priorities are:

1. Make the producer experience significantly lower latency.
2. Reduce unnecessary decode/re-encode stages.
3. Improve efficiency and scalability.
4. Add optional low-latency viewer output.
5. Add NDI support in a sensible way without treating NDI as a WAN guest protocol.

Do not try to implement everything in one PR.

Prefer small, independently testable changes that remain easy to rebase onto upstream Muxshed.

---

# Current approximate media architecture

Remote browser guest:

    Browser
       |
       | WebRTC / WHIP
       | VP8/H.264 + Opus
       v
    Muxshed WebRTC ingest
       |
       | ffmpeg normalisation
       v
    H.264 + AAC / FLV source relay
       |
       +-------------------------------+
       |                               |
       v                               v
    source preview                 scene compositor
    WS-FLV/mpegts.js                   |
                                       | ffmpeg decode/composite/re-encode
                                       v
                                   scene FLV
                                       |
                                       v
                                  program router
                                       |
                      +----------------+----------------+
                      |                |                |
                      v                v                v
                    RTMP             HLS            recording
                                      |
                                      v
                                 public watch page

There are currently potentially multiple encode/decode stages before output.

The producer preview also uses WS-FLV / `mpegts.js`, which introduces noticeably more latency than WebRTC.

---

# Overall design direction

Long-term preferred topology:

    Remote browser guest ---- WHIP/WebRTC -----+
                                                |
    Remote OBS ------------ WHIP/SRT ----------+
                                                |
    Venue LAN camera ---- NDI ---- edge bridge -+
                                                |
                                                v
                                         MUXSHED CORE
                                                |
                              +-----------------+------------------+
                              |                                    |
                              v                                    v
                       low-latency producer                  broadcast outputs
                       WebRTC/WHEP preview             RTMP / SRT / HLS / etc.
                              |
                              v
                         Browser Studio

Keep:

- WHIP/WebRTC for browser guests.
- SRT for robust WAN contribution.
- RTMP for compatibility.
- NDI primarily for LAN/venue contribution.
- HLS for scalable public viewing.

---

# PHASE 1 — WebRTC / WHEP producer monitoring

## Problem

The Studio currently previews sources using something equivalent to:

    source relay
        |
        v
    WebSocket FLV
        |
        v
    mpegts.js
        |
        v
    HTMLVideoElement

This gives noticeably higher latency than expected for a browser production system.

The producer should ideally see:

- guests
- sources
- Preview
- Program

with sub-second latency.

---

## Objective

Add a low-latency WebRTC playback path for producer monitoring.

Preferred protocol:

    WHEP

or a small WHEP-compatible WebRTC receive endpoint.

Do not replace the existing WS-FLV path immediately.

Implement WebRTC preview as an optional/new path first, retaining WS-FLV as fallback.

---

## Desired architecture

Current:

    Source
      |
      v
    FLV relay
      |
      v
    WS-FLV
      |
      v
    browser

Desired:

    Source / Program
         |
         v
    WebRTC playback endpoint
         |
         | WHEP
         v
      browser

Target producer glass-to-glass latency:

    ideally < 500 ms
    acceptable initial target < 1 second

---

## Required playback targets

Support WebRTC monitoring for:

1. Individual source preview.
2. Program monitor.
3. Preview/Next-Up monitor.

Program is highest priority.

Source thumbnails may remain WS-FLV initially if doing all sources at once creates excessive WebRTC connections.

---

## WHEP endpoint concept

Possible API shape:

    POST /api/v1/sources/<source-id>/whep

and:

    POST /api/v1/program/whep

Request:

    Content-Type: application/sdp

Body:

    WebRTC SDP offer

Response:

    SDP answer

Follow WHEP semantics as closely as practical.

Do not invent an unnecessarily proprietary protocol if WHEP can be used cleanly.

---

## Codec considerations

For browser playback, prefer:

Video:

    H.264 where browser-compatible
    VP8 fallback if necessary

Audio:

    Opus

Do not transcode to AAC merely for the WebRTC producer monitor.

If the internal programme path is AAC/H.264 FLV, investigate whether:

- audio requires AAC -> Opus transcoding for WHEP
- video H.264 can be repacketised rather than transcoded

Avoid unnecessary video transcoding where possible.

---

## Browser behaviour

Producer monitors should:

- start muted by default
- have explicit local audio monitor controls
- not violate autoplay restrictions
- allow Program audio monitoring
- support one-at-a-time PFL in future

Do not automatically unmute multiple sources.

---

## Fallback

If WebRTC monitoring cannot connect:

    fall back to WS-FLV preview

Do not make the Studio unusable because WHEP fails.

---

## Acceptance criteria

Program preview:

- starts within ~1 second
- remains stable during source changes
- audio/video remain in sync
- survives guest reconnect
- survives scene changes
- survives failover/failback

Compare measured latency against current WS-FLV monitor.

Document results.

---

# PHASE 2 — Reduce unnecessary transcoding

## Problem

Current guest contribution may pass through several generations:

    Browser VP8/Opus
        |
        v
    ffmpeg
        |
        v
    H.264/AAC
        |
        v
    scene ffmpeg
        |
        v
    H.264/AAC
        |
        v
    HLS ffmpeg
        |
        v
    H.264/AAC

This costs:

- CPU
- latency
- quality
- scalability

---

## Objective

Audit the entire media path and identify where:

- decoding is mandatory
- encoding is mandatory
- packet remux/repacketisation would suffice

Do not blindly optimise before establishing current data flow.

---

## Audit deliverable

For each path, document:

    input codec
    decode?
    filter/composite?
    encode?
    output codec

For example:

    WHIP guest
      VP8/Opus
      -> decode video
      -> H.264 encode
      -> AAC encode

    H.264 WHIP guest
      H.264/Opus
      -> is H.264 decoded?
      -> is it re-encoded unnecessarily?
      -> could H.264 be passed through?

---

## Desired optimisation

If a WHIP publisher sends compatible H.264:

    H.264
      |
      +---- no resize/composite required ----> packetise/remux
      |
      +---- scene composition required ------> decode -> composite -> encode

Avoid an initial full H.264 re-encode merely to turn an H.264 WebRTC contribution into FLV if it can safely be remuxed/repacketised.

Audio may still require:

    Opus -> AAC

for current FLV/RTMP compatibility.

Keep future architecture open to retaining Opus deeper into the system.

---

# PHASE 3 — Scene compositor optimisation

## Problem

A scene currently introduces an additional ffmpeg compositor and H.264 encode.

That may be unavoidable for arbitrary compositing, but we should minimise latency.

---

## Objective

Optimise the scene compositor for live production rather than file encoding.

Audit:

- ffmpeg buffering
- probe sizes
- GOP
- B-frame behaviour
- queue sizes
- input buffering
- filter latency
- thread queues
- mux buffering

Current encode settings already include:

    -preset veryfast
    -tune zerolatency

Retain or improve low-latency settings.

---

## Possible improvements

Investigate appropriately:

    -bf 0
    -flags low_delay
    reduced analyzeduration
    reduced probesize
    flush_packets
    muxdelay / muxpreload where applicable

Do not apply flags blindly.

Measure before/after latency and stability.

---

## Scene architecture

Continue separating:

    scene visual composition

from:

    programme audio routing

where practical.

Do not couple latency optimisation to a rewrite of the audio mixer.

---

# PHASE 4 — Hardware encoding

## Objective

Allow Muxshed to use hardware encode/decode when available.

Initial targets:

1. Intel VAAPI / Quick Sync
2. NVIDIA NVENC/NVDEC

AMD VAAPI support is desirable where practical.

---

## Configuration

Do not hard-code hardware acceleration.

Add optional configuration such as:

    MUXSHED_HWACCEL=none
    MUXSHED_HWACCEL=vaapi
    MUXSHED_HWACCEL=qsv
    MUXSHED_HWACCEL=nvenc

Possibly auto-detect later.

Default:

    software

---

## Docker support

Document required device mappings.

VAAPI example:

    devices:
      - /dev/dri:/dev/dri

NVIDIA:

Use the standard NVIDIA container runtime approach.

Do not make the base image require a GPU.

---

## Encoder selection concept

Software:

    libx264

VAAPI:

    h264_vaapi

Quick Sync:

    h264_qsv

NVIDIA:

    h264_nvenc

Use sensible low-latency encoder options for each backend.

---

## Goals

Hardware acceleration should primarily improve:

- CPU capacity
- number of simultaneous guests
- number of scenes
- thermal/load characteristics

Latency improvements are welcome but must be measured rather than assumed.

---

# PHASE 5 — Low-latency public viewer mode

## Existing behaviour

Public Channel currently uses HLS:

    H.264 + AAC
    2-second MPEG-TS segments

This is appropriate for scalable public viewing but inevitably adds several seconds latency.

Do NOT remove normal HLS.

---

## Desired addition

Add optional low-latency public viewing using WebRTC/WHEP.

Possible modes:

    Standard
        HLS

    Low Latency
        WebRTC/WHEP

Potential UI:

    Public Channel
    [x] Standard HLS viewer
    [ ] Enable low-latency WebRTC viewer

---

## Scaling warning

WebRTC public viewing does not scale like HLS.

The implementation should make that clear.

Do not silently replace HLS with one peer connection per public viewer.

Use cases for low-latency viewer:

- private meetings
- small interactive events
- confidence monitors
- speaker return
- operator/producer monitoring

Use HLS for:

- large audiences
- general public streams

---

## Later possibility

LL-HLS may eventually provide an intermediate option.

Do not implement LL-HLS in the same PR as WHEP.

---

# PHASE 6 — NDI support

## Important architectural rule

NDI is primarily a LAN/venue production protocol.

Do not treat full-bandwidth NDI as a replacement for:

    WHIP
    SRT

for normal remote Internet contribution.

Muxshed is cloud-hosted, so direct NDI over WAN should not be the default architecture.

---

# NDI use cases

Good use cases:

    venue cameras
    OBS machines on venue LAN
    presentation laptops
    graphics machines
    local playout machines
    PTZ cameras
    hardware converters

Example:

    Camera
      |
      | NDI
      v
    venue LAN
      |
      v
    local Muxshed edge / bridge
      |
      | WHIP or SRT
      v
    cloud Muxshed

---

# Preferred NDI architecture

Do not begin by embedding the complete NDI SDK deep inside the Muxshed core.

Prefer an isolated receiver/bridge architecture.

Concept:

    NDI source
        |
        v
    NDI receiver service
        |
        +--> WHIP
        |
        or
        |
        +--> SRT
        |
        v
    normal Muxshed source

This gives us:

- isolation from NDI SDK concerns
- easier licensing/distribution
- less invasive upstream divergence
- ability to run the bridge at the venue
- existing Muxshed ingest paths remain reusable

---

# NDI edge service

Potential project:

    muxshed-ndi-bridge

Responsibilities:

1. Discover NDI sources.
2. Allow selection of one source.
3. Receive embedded audio/video.
4. Encode/transcode only where required.
5. Publish to Muxshed using:
   - WHIP preferred for low latency
   - SRT optional for difficult WAN links

Possible configuration:

    NDI_SOURCE="CAMERA 1 (PTZ)"
    MUXSHED_URL=https://studio.example.com
    WHIP_URL=...
    TOKEN=...

---

# NDI discovery

NDI normally uses local discovery.

Support:

    mDNS/local discovery

and optionally:

    NDI Discovery Server

Do not expect multicast discovery to work transparently across WAN/cloud networks.

---

# NDI variants

Design with awareness of:

    High Bandwidth NDI
    NDI HX
    NDI HX2 / HX3

Do not assume all NDI streams are uncompressed/high-bandwidth.

Where an NDI HX stream already contains H.264/H.265, investigate whether encoded payload can be preserved rather than always decoding/re-encoding.

Only implement this if the SDK/API makes it practical.

---

# Direct NDI source inside Muxshed — later option

Eventually Muxshed's Add Source UI could support:

    Add Source

    RTMP
    SRT
    WHIP
    Browser
    NDI

Possible NDI configuration:

    Source:
    [ CAMERA 1 (NDI)              ▼ ]

    Audio:
    [x] Embedded audio

    Quality:
    [ Highest / Auto             ▼ ]

But this should come after the external bridge prototype proves the media path.

---

# NDI licensing / packaging

Treat NDI SDK redistribution separately from normal open-source dependencies.

Before committing:

- inspect current NDI SDK licence
- identify runtime redistribution requirements
- do not commit proprietary SDK binaries to the public fork without permission
- avoid assuming standard distro ffmpeg contains NDI support

Prefer keeping NDI runtime integration isolated from the base Muxshed image.

Document all requirements.

---

# PHASE 7 — Edge ingest concept

A useful longer-term feature would be a lightweight Muxshed Edge Agent.

Example:

    venue LAN
    ─────────────────────────────

    NDI Camera ─┐
    OBS ────────┤
    HDMI card ──┤
                v
         Muxshed Edge
                |
                | WHIP / SRT
                v

    Internet
    ═════════════════════════════

                |
                v
          Cloud Muxshed

The edge service could eventually provide:

- NDI discovery
- local capture cards
- local RTMP ingest
- transcoding
- WAN bonding/reconnect logic
- SRT contribution
- WHIP contribution

This is a future project, not required immediately.

---

# Latency measurement

Do not optimise based solely on perception.

Create a simple repeatable latency test.

Suggested method:

Display a millisecond stopwatch / timecode in front of a camera.

Photograph or screen-capture:

    original clock
    producer source preview
    program preview
    public viewer

Compare timestamps.

Record:

    guest -> source monitor
    guest -> program monitor
    guest -> HLS viewer
    guest -> WebRTC viewer

Measure before and after each major change.

---

# Target latency goals

These are directional targets, not strict guarantees.

Remote WHIP guest to producer source preview:

    < 500 ms desirable
    < 1 s acceptable

Remote guest to Program monitor:

    < 750 ms desirable

Venue NDI -> edge -> cloud Program:

    < 1 s desirable

Public HLS:

    several seconds acceptable

Public WebRTC viewer:

    < 1 s desirable

---

# CPU / capacity measurements

For each architecture change also record:

    muxshed-api CPU
    guest ffmpeg CPU
    scene compositor CPU
    HLS ffmpeg CPU
    memory
    encoder fps
    dropped frames

We currently observed multiple ffmpeg jobs each consuming substantial CPU.

Optimisation should improve capacity as well as latency.

---

# Important non-goals

Do NOT:

- replace WHIP guest ingest with NDI
- send full-bandwidth NDI over the public Internet by default
- remove HLS
- require a GPU
- require proprietary NDI components in the base image
- rewrite all media handling in one PR
- merge WHEP, NDI, hardware encode and compositor optimisation into one giant change
- sacrifice stability merely to save a few milliseconds

---

# Proposed implementation order

## PR 1 — WebRTC Program Monitor

Implement:

    Program -> WHEP/WebRTC -> browser

Retain WS-FLV fallback.

Measure latency.

---

## PR 2 — Source WebRTC Preview — **done (guestux.8)**

WHEP for Preview / selected source (`POST /api/v1/sources/{id}/whep`).

Thumbnails stay on WS-FLV (no per-tile encoder). Program + Preview share `WhepMonitor`.

---

## PR 3 — Media Pipeline Audit / Pass-through — **done (guestux.9)**

See `PIPELINE.md`. RTMP egress defaults to `-c copy`. H.264 WHIP remuxes video.
`OutputConfig.transcode_egress` restores letterbox encode when needed.

---

## PR 4 — Scene Latency Optimisation — **done (guestux.9)**

Compositor low-delay flags + single-layer identity forward (no encode).

---

## PR 5 — Hardware Encoding — **deferred**

Not required for producer snappiness under the no-transcode model. Do not add
VAAPI/NVENC merely to make grid tiles feel live.

---

## PR 6 — Low-latency public WHEP viewer — **deferred**

Keep HLS as the public default.

---

## PR 7 — NDI Bridge Prototype — **out of scope**

---

## PR 8 — NDI UI integration — **out of scope**

---

# Deliverables for initial work

Start with PR 1 only.

Produce:

1. WebRTC/WHEP Program playback endpoint.
2. Browser-side Program monitor player.
3. WS-FLV fallback.
4. Program audio monitoring support.
5. Clean peer teardown/reconnect.
6. Source-switch survival.
7. Basic latency measurements before/after.
8. Tests where practical.
9. Documentation.
10. Update `FORK_NOTES.md`.

Before implementing later phases, leave design notes/TODOs but do not prematurely build them.

---

# Architectural principle

The long-term goal is:

    contribution protocols
        |
        v
    normalised media sources
        |
        v
    low-latency programme core
        |
        +---- WebRTC monitoring
        |
        +---- HLS
        |
        +---- RTMP/SRT
        |
        +---- recording

Protocol-specific ingest should terminate near the edge.

The internal programme architecture should not become tightly coupled to:

    RTMP
    WHIP
    NDI
    SRT

This keeps future source types straightforward.

---

# End-state vision

A future Muxshed instance should comfortably support:

    Browser guest -------- WHIP --------+
                                         |
    OBS ------------------ WHIP/SRT -----+
                                         |
    Remote encoder ------- SRT ----------+
                                         |
    NDI camera --- Edge --- WHIP/SRT -----+
                                         |
                                         v
                                  MUXSHED STUDIO
                                         |
                    +--------------------+---------------------+
                    |                    |                     |
                    v                    v                     v
              WebRTC producer         HLS viewer          RTMP/SRT
                 monitor                                  destinations
                    |
                    v
             low-latency browser
               production UI

The immediate priority is making the producer side feel responsive enough to operate as a real live-production switcher.