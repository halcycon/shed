# Muxshed Guest UX Fork — Background Blur / Green Room Enhancements

## Project context

We are running a small fork of:

- Upstream repository: `muxshed/shed`
- Current deployed version: `1.8.6`
- Deployment: Docker via Arcane
- Current image: `ghcr.io/muxshed/shed:1.8.6`
- Public studio URL is HTTPS behind Cloudflare Tunnel.
- Guests join using `/guest/<token>`.
- TURN/STUN is already configured and working.
- TURN hostname currently resolves to a public coturn instance.
- Guest contribution is WebRTC/WHIP.

The intention is to keep this fork as small as possible, regularly merge/rebase upstream Muxshed, and confine most customisation to the guest-facing browser frontend.

Do not unnecessarily rewrite Muxshed backend functionality.

---

# Current upstream implementation

The primary guest page is:

`web/src/routes/guest/[token]/+page.svelte`

Current behaviour is approximately:

1. Fetch guest metadata/ICE configuration:
   `GET /api/v1/guest/<token>`
2. Request camera + microphone:
   `navigator.mediaDevices.getUserMedia(...)`
3. Display the raw camera stream in the preview `<video>`.
4. Create:
   `new RTCPeerConnection({ iceServers })`
5. Add audio/video tracks.
6. Gather ICE candidates.
7. POST the SDP offer to:
   `/api/v1/guest/<token>/whip`
8. Apply the SDP answer.
9. Show "You are live in the studio" once the peer connection reaches `connected`.
10. Device changes use `RTCRtpSender.replaceTrack()`.

Preserve this overall mechanism.

The existing code structure is useful because `replaceTrack()` makes it possible to switch between raw and background-processed video without disconnecting the guest.

---

# Codec note — do NOT "fix" G.722

Do not alter the audio codec stack unless a real problem is found.

Chromium's SDP *offer* contains several codecs, including:

- Opus PT 111
- RED
- G.722
- PCMU
- PCMA

This does NOT mean G.722 is being used.

Muxshed's current backend explicitly registers:

- Opus
- 48 kHz
- stereo
- payload type 111
- `minptime=10;useinbandfec=1`

Its ingest bridge also expects Opus audio.

Therefore the negotiated Muxshed guest audio should already be Opus.

As part of testing, confirm that the SDP answer selects Opus. Do not add codec-preference manipulation unless this test disproves the above.

---

# Main objective

Improve the guest join page into a lightweight StreamYard/Jitsi-style "green room", most importantly adding:

1. Background blur
2. Camera preview
3. Camera selector
4. Microphone selector
5. Live microphone level meter
6. Camera on/off
7. Microphone mute/unmute
8. Clean switching of background processing while already connected

Background effects MUST happen entirely in the guest's browser.

Do NOT upload raw video to a server for processing.

---

# Required UI

The guest page should approximately become:

    ┌──────────────────────────────────────────┐
    │              MUXSHED GUEST               │
    │                                          │
    │       ┌──────────────────────────┐       │
    │       │                          │       │
    │       │      Camera Preview      │       │
    │       │                          │       │
    │       └──────────────────────────┘       │
    │                                          │
    │ Camera       [ Camera name           ▾ ] │
    │ Microphone   [ Microphone name       ▾ ] │
    │                                          │
    │ Background   [ None ] [ Blur ]           │
    │                                          │
    │ Camera       [ On / Off ]                 │
    │ Microphone   [ Mute / Unmute ]            │
    │                                          │
    │ Mic level    ████████░░░░                 │
    │                                          │
    │          [ Join the broadcast ]           │
    └──────────────────────────────────────────┘

When connected, retain the appropriate disconnect/live-state controls.

Preserve Muxshed's existing visual style unless changing a component is necessary.

---

# Background blur architecture

## Requirement

Implement person/background segmentation locally in the browser.

Preferred implementation:

- MediaPipe Tasks Vision `ImageSegmenter`, or another actively maintained browser-side person/selfie segmentation implementation.
- Prefer WebGL/WASM acceleration where supported.
- Verify licensing of any model files before committing them to the repository.
- Do not introduce a cloud inference dependency.

Abstract the segmentation engine behind a small interface so we are not permanently tied to one implementation.

Suggested structure:

    web/src/lib/video-effects/
        background-processor.ts
        mediapipe-segmenter.ts

or equivalent appropriate to the existing Svelte codebase.

Suggested conceptual interface:

    interface BackgroundProcessor {
        initialise(): Promise<void>;
        process(source: HTMLVideoElement): void;
        getTrack(): MediaStreamTrack;
        destroy(): void;
    }

Exact API is flexible.

---

# Video flow

Current:

    getUserMedia()
         |
         +---- camera track ----------> RTCPeerConnection
         |
         +---- microphone track ------> RTCPeerConnection

New:

                        +---- None --------------------+
                        |                              |
    getUserMedia() -----+                              +--> selected video track
                        |                              |
                        +--> segmentation --> canvas --+
                                                |
                                      canvas.captureStream()

    microphone track -------------------------------> audio sender

The raw microphone track must never pass through the video processing path.

---

# Canvas composition

For blur mode:

1. Capture camera frame.
2. Run person segmentation.
3. Draw the complete camera frame to a canvas with a configurable blur filter.
4. Composite the foreground/person over it without blur.
5. Export the canvas as a MediaStream using `canvas.captureStream()`.

Target:

- output up to 30 fps
- preserve camera aspect ratio
- avoid unnecessarily processing 4K frames

Prefer processing at a sensible resolution, e.g. 720p output with lower-resolution segmentation masks if necessary.

The user's CPU/GPU should do this work, not the Muxshed server.

---

# Performance requirements

Background blur must not make the guest page unusable on ordinary laptops.

Implement sensible performance behaviour:

- Prefer `requestVideoFrameCallback()` when available.
- Fall back to `requestAnimationFrame()` if necessary.
- Target 24–30 processed fps.
- Do not run segmentation faster than required.
- It is acceptable to perform segmentation at reduced resolution and upscale the mask.
- Avoid allocating new canvas/image objects every frame.
- Clean up all loops, media tracks and model instances on disconnect/page unload.

If performance is too poor:

1. Reduce segmentation resolution before reducing output resolution.
2. Never silently freeze the outgoing video.

---

# Effect modes

Required for first implementation:

    None
    Blur

Architecture should allow later addition of:

    Strong Blur
    Background Image
    Background Colour

But those are NOT required for the first PR.

A blur strength constant/config value is sufficient initially.

---

# Track management

This is important.

Separate the concepts of:

    rawCameraTrack
    processedVideoTrack
    microphoneTrack
    outboundVideoTrack

Do not treat the entire MediaStream as one opaque object.

When changing from:

    None -> Blur

while already connected:

1. Start the processor.
2. Obtain processed track.
3. Find the RTCRtpSender whose track kind is `video`.
4. `sender.replaceTrack(processedVideoTrack)`.
5. Update the preview.

When changing:

    Blur -> None

1. `replaceTrack(rawCameraTrack)`
2. Stop/destroy the processed video track.
3. Destroy segmentation/render loop.
4. Update preview.

This should NOT require WHIP renegotiation or reconnection.

---

# Camera switching

Existing Muxshed already supports switching devices while connected.

Preserve that functionality.

When blur is OFF:

    switch camera
      -> replace raw camera track normally

When blur is ON:

    switch camera
      -> obtain new camera stream
      -> reconnect processor to new source
      -> create/use new processed track
      -> replace outgoing video sender with processed track

Do not accidentally switch the sender back to the unprocessed camera when the user has blur enabled.

---

# Camera disable

Add a guest-facing camera toggle.

Preferred first implementation:

    videoTrack.enabled = false

rather than destroying/recreating the entire peer connection.

The preview should clearly indicate that the camera is disabled.

If the processed track is active, correctly disable the track that is actually being transmitted.

---

# Microphone mute

Add a microphone toggle.

Implementation should normally be:

    microphoneTrack.enabled = false

Do not remove/re-add the track unless technically necessary.

Show an obvious muted state.

---

# Microphone level meter

Implement a local pre-join/live microphone meter using the Web Audio API.

Suggested flow:

    MediaStream audio track
          |
          v
    AudioContext
          |
    MediaStreamAudioSourceNode
          |
       AnalyserNode
          |
    UI level meter

Do NOT connect it to `AudioContext.destination`, otherwise local microphone monitoring/echo may occur.

The meter may use RMS or peak amplitude.

Requirements:

- smooth enough to show speech
- visually indicate silence vs speech
- continue working before joining
- continue working while connected
- destroy AudioContext/animation loop correctly on teardown

No server-side support is needed.

---

# Preview behaviour

The preview should show what the guest is actually transmitting.

Therefore:

    Blur OFF -> raw camera preview
    Blur ON  -> processed canvas stream preview

The guest should be able to inspect the blur before joining.

Prefer local preview mirroring if desired for usability, but mirroring should be PRESENTATIONAL ONLY.

Do not unintentionally send horizontally mirrored video to the producer unless explicitly designed.

---

# Failure behaviour

Effects must be optional.

If segmentation cannot initialise because of:

- unsupported browser
- WebGL/WASM issue
- model download failure
- insufficient resources
- runtime exception

then:

1. Fall back to raw camera video.
2. Keep camera/mic and joining functionality working.
3. Disable or hide Blur.
4. Show a small non-fatal message such as:
   "Background effects are not available in this browser."

Do NOT prevent the user joining a broadcast because blur failed.

---

# Browser support

Primary supported browsers:

1. Chromium / Google Chrome
2. Microsoft Edge

Firefox may continue to work but is secondary.

We have already observed visual corruption with at least one Linux/Firefox webcam configuration while Chromium works correctly.

Do not deliberately break Firefox, but do not design around Firefox-specific limitations at the expense of Chromium.

Use feature detection rather than user-agent sniffing wherever possible.

---

# Privacy

Background processing must be local.

No video frames should be:

- uploaded to third-party AI services
- persisted
- sent anywhere other than the existing Muxshed WebRTC connection

If MediaPipe/model assets are downloaded from a CDN, consider instead serving them from Muxshed/static assets so guest camera processing does not create an unnecessary third-party dependency.

Document what assets/models are used.

---

# Keep the fork upstream-friendly

This is important.

Avoid broad refactoring of unrelated Muxshed code.

Prefer:

    web/src/routes/guest/[token]/+page.svelte

plus small reusable helpers under:

    web/src/lib/video-effects/
    web/src/lib/audio/

Do not alter:

- source switching
- output pipeline
- ffmpeg pipeline
- RTMP
- SRT
- scenes
- authentication
- database schema

unless strictly necessary.

The goal is for future upstream merges to be straightforward.

---

# Optional componentisation

If the current guest page becomes unwieldy, extracting the following is encouraged:

    GuestPreview.svelte
    DeviceSelector.svelte
    AudioMeter.svelte
    BackgroundControls.svelte

But do not componentise merely for its own sake.

---

# State model suggestion

Consider explicit state similar to:

    rawStream
    rawVideoTrack
    microphoneTrack

    effectsMode = "none" | "blur"

    processor
    processedStream
    processedVideoTrack

    cameraEnabled
    microphoneEnabled

    pc

Provide helper functions such as:

    acquireDevices()
    rebuildVideoPipeline()
    setBackgroundMode()
    replaceOutgoingVideoTrack()
    setCameraEnabled()
    setMicrophoneEnabled()
    initialiseAudioMeter()
    teardownMedia()

Avoid multiple functions independently starting/stopping the same camera tracks.

---

# Important lifecycle case

The existing device-switch implementation stops the current stream before acquiring another.

Be careful when introducing processing because stopping a source camera track underneath a canvas processor may leave:

- dead captureStream tracks
- running render loops
- leaked MediaPipe instances
- stale RTCRtpSender tracks

Ensure switch-device flow explicitly tears down or rebuilds the processing pipeline.

---

# WHIP behaviour

Do not redesign the WHIP protocol.

Existing behaviour should remain:

    RTCPeerConnection
        |
        + add audio/video tracks
        |
        + createOffer()
        |
        + setLocalDescription()
        |
        + wait for ICE gathering
        |
        + POST SDP to:
          /api/v1/guest/<token>/whip
        |
        + receive SDP answer
        |
        + setRemoteDescription()

The only substantive change is which video MediaStreamTrack gets attached/sent.

---

# TURN / ICE

Do not alter TURN handling.

Muxshed currently fetches `ice_servers` from the guest API and passes them into:

    new RTCPeerConnection({ iceServers })

This is already working.

Observed ICE candidates include:

- LAN host
- Tailscale host
- public STUN/srflx
- public TURN relay

TURN is already proven to allocate relay candidates successfully.

Preserve this behaviour.

---

# Audio verification test

As part of implementation/testing:

1. Join as Chromium guest.
2. Capture the `/guest/<token>/whip` request and response.
3. Verify the outgoing offer contains Opus.
4. Verify the Muxshed SDP answer chooses Opus/PT111.
5. Confirm live audio reaches Muxshed.

Do NOT force G.722 removal from the browser offer simply because it appears there.

---

# Functional acceptance tests

## Basic guest

- Open guest link.
- Browser requests camera/mic permission.
- Camera preview works.
- Camera selector works.
- Microphone selector works.
- Join succeeds.
- Producer receives audio/video.
- Disconnect works.

## Blur before joining

- Select Blur.
- Preview becomes blurred-background image.
- Person remains substantially sharp.
- Join.
- Producer receives processed video, not raw video.

## Blur while live

- Join with None.
- Enable Blur while live.
- No disconnect.
- Producer video switches to blurred version.
- Disable Blur.
- Producer returns to raw video.
- No renegotiation failure.

## Camera switch with blur

- Enable Blur.
- Change camera.
- Preview changes.
- Processed output uses new camera.
- Producer does not receive raw background accidentally.

## Microphone

- Level meter responds to speech.
- Mute stops transmitted audio.
- Meter state/UI reflects mute appropriately.
- Unmute restores audio without reconnecting.

## Camera toggle

- Camera Off stops/hides transmitted video appropriately.
- Camera On restores it.
- No full guest reconnect required.

## TURN

No regressions to ICE configuration.

Guest should continue gathering relay candidates from the configured TURN server.

## Failure mode

Artificially prevent segmentation model loading.

Expected:

- warning shown
- Blur unavailable
- raw camera continues to work
- guest can still join normally

---

# Performance acceptance

Test on Chromium/Linux.

During blur:

- no runaway memory growth
- no continuously growing number of MediaStreamTracks
- no growing number of AudioContexts
- no duplicate rendering loops
- usable video frame rate
- CPU load is reasonable for a browser background effect

Toggle Blur on/off at least 20 times and ensure old resources are released.

Switch cameras several times and check for leaks.

---

# Developer diagnostics

During development, useful optional console diagnostics are acceptable for:

- selected camera settings
- selected microphone settings
- source resolution/fps
- processing resolution
- blur processing fps
- selected outbound video track ID

Remove excessive per-frame logging before production.

---

# Build / deployment

Do not deploy directly to production first.

Create a fork/branch and ensure the normal upstream build process still succeeds.

Expected eventual Docker image naming could be:

    ghcr.io/<our-account>/shed:merlin

or preferably versioned:

    ghcr.io/<our-account>/shed:1.8.6-merlin.1

Deployment in Arcane would then change only:

    image: ghcr.io/muxshed/shed:1.8.6

to:

    image: ghcr.io/<our-account>/shed:1.8.6-merlin.1

Keep image tags immutable/versioned for production.
Do not rely solely on `latest`.

---

# Suggested development phases

## Phase 1

Refactor media state cleanly without functional changes.

Ensure:

- camera switch
- microphone switch
- join
- disconnect

all still work.

## Phase 2

Add microphone level meter and mic/camera toggles.

## Phase 3

Add background processor abstraction.

Implement Blur.

## Phase 4

Support changing effect while connected using `replaceTrack()`.

## Phase 5

Performance/lifecycle cleanup and browser testing.

## Phase 6

Build custom Docker image and test via Arcane.

---

# Non-goals for this PR

Do NOT implement yet:

- virtual backgrounds/images
- generative backgrounds
- beauty filters
- face tracking
- server-side segmentation
- recording changes
- output codec changes
- scenes/layout changes
- producer-side effects
- new authentication
- TURN credential redesign
- Jitsi integration
- VDO.Ninja integration

Those can be separate work.

---

# Deliverables

Please produce:

1. Working code.
2. Short summary of changed files.
3. Any added npm dependencies and their licences.
4. Any required model/static assets and their licences.
5. Build/test instructions.
6. Screenshots or notes showing:
   - raw preview
   - blur preview
   - live blur toggle
7. Confirmation that upstream guest WHIP behaviour still works.
8. Confirmation from SDP negotiation that audio remains Opus.
9. A short `FORK_NOTES.md` documenting the custom changes so future upstream rebases are straightforward.

Before making broad architectural changes, favour the smallest implementation that satisfies the requirements above.