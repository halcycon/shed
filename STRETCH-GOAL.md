## Producer audio monitoring addition

The existing `VideoPreview.svelte` hard-codes:

    <video ... muted playsinline>

Therefore all Studio source/program previews are silent even though their
WebSocket-FLV streams contain audio.

Add local producer monitoring without changing broadcast audio routing.

Requirements:

- Keep previews muted by default.
- Add an `audioEnabled` / `monitorAudio` prop to `VideoPreview`.
- Do not automatically unmute because browser autoplay policy may block it.
- Program monitor gets a user-operated "Monitor Audio" speaker toggle.
- Clicking the toggle is the user gesture used to enable playback.
- Monitoring is browser-local only and MUST NOT change Muxshed's
  `AudioRouting`, programme source, or outgoing stream.
- Do not enable audio on all thumbnails simultaneously.
- Later support one-source-at-a-time PFL/headphone monitoring.

Also replace the current fake audio meters. They currently use Math.random()
and do not represent actual levels.

Use Web Audio API / AnalyserNode against the media element to obtain real
RMS/peak levels where practical.

Avoid routing producer microphone audio into this monitoring graph and
document that headphones are strongly recommended to avoid acoustic feedback.

# Muxshed Fork Follow-up — Fix Public HLS Startup and Scene Audio

## Context

We are maintaining a small fork of:

- Upstream: `muxshed/shed`
- Current deployed upstream version: `1.8.6`
- Deployment: Docker via Arcane
- Public studio accessed over HTTPS through Cloudflare Tunnel
- Guest ingest uses WebRTC/WHIP
- TURN is working
- ffmpeg is present inside the Muxshed Docker image

Do not redesign unrelated parts of Muxshed.

This task concerns two concrete issues discovered during live testing:

1. Public Channel HLS can take several minutes to begin generating segments after the HLS encoder starts.
2. Scene composites currently output silence instead of the appropriate source audio.

There is also one trivial stale UI text issue.

---

# Issue 1 — Public Channel HLS startup is unreliable / very delayed

## Observed behaviour

Studio was already ON AIR.

Public Channel was enabled.

Watch page remained:

    OFFLINE
    Waiting for the broadcast to start...

Muxshed showed:

    Public channel: LIVE
    Studio: ON AIR

The HLS ffmpeg process existed:

    ffmpeg
      -f flv
      -i pipe:0
      ...
      -f hls
      -hls_time 2
      ...
      /config/hls/<token>/index.m3u8

However:

    /config/hls/<token>/

remained completely empty for several minutes.

Eventually, without further configuration changes, HLS files appeared:

    index.m3u8
    seg00000.ts
    seg00001.ts
    ...
    seg00006.ts

The segments were ~1 MB each and the playlist began updating.

With `-hls_time 2`, startup should happen in seconds, not minutes.

---

# Relevant architecture

Muxshed has:

    source media relay
          |
          v
    program router
          |
          v
      program_tx
        /     \
       /       \
    egress    ChannelHls
               |
               v
             ffmpeg
               |
               v
       index.m3u8 + .ts

`ChannelHls` subscribes to `program_tx` and feeds an FLV stream to ffmpeg stdin.

The HLS implementation lives around:

    crates/api/src/channel_hls.rs

Stream start / HLS lifecycle:

    crates/api/src/routes/stream.rs

Program routing:

    crates/api/src/program.rs

Failover / effective program source:

    crates/api/src/failover.rs

---

# Important distinction

Muxshed has:

    program_intent

and:

    program_source

`program_intent` is what the operator wants on Program.

`program_source` is what is actually routed, after failover logic.

Do not assume they are always synchronously identical.

However, testing has shown that the Program Router *was already forwarding correctly* before Channel HLS was started.

Relevant logs:

    program router: video=<uuid> audio=<uuid> (same=true)

and source switches were completing normally:

    program router: preparing switch ...
    program router: switched to ... after N frames
    program router: video=... audio=... (same=true)

So this is not simply "program_source was None".

---

# HLS diagnostics observed

Muxshed logs showed:

    starting channel HLS →
    /config/hls/<token>/index.m3u8

The ffmpeg process remained alive.

There were no obvious:

    ffmpeg [channel]: ...

errors.

The process appeared idle until it eventually began writing segments.

Therefore investigate startup synchronisation around:

- FLV sequence headers
- first decodable keyframe
- subscriber joining an already-running `program_tx`
- source switching
- stream timestamps
- whether ChannelHls starts between useful FLV packets
- whether it can receive audio/video tags before codec config

---

# Likely design flaw

`program_tx` is a broadcast channel carrying live FLV tags.

A new ChannelHls subscriber joins **mid-stream**.

A mid-stream subscriber may therefore begin with arbitrary packets such as:

    P-frame
    audio frame
    non-header FLV packet

instead of receiving:

    FLV header
    AVC/AAC sequence headers
    keyframe
    subsequent packets

ChannelHls currently writes a fresh FLV header itself and optionally writes cached
sequence headers before consuming `program_tx`.

This makes correctness dependent on the cached headers being:

- present
- current
- appropriate for the actual routed programme
- supplied before the next decodable keyframe

That appears insufficiently robust.

---

# Required HLS fix

Make ChannelHls deterministic when it attaches to an already-running programme.

The public channel should become playable within approximately:

    2–6 seconds

under normal operation.

Do NOT solve this with:

    sleep(5000)

or arbitrary startup delays.

---

# Preferred approach

When ChannelHls starts:

1. Resolve the current effective `program_source`.
2. Obtain current video/audio sequence headers for the programme.
3. Subscribe to `program_tx`.
4. Write:
   - FLV header
   - current video sequence header
   - current audio sequence header
5. Ignore/drop video frames until the first keyframe is received.
6. Start feeding decodable media into ffmpeg from that keyframe onward.

Alternatively, make the programme bus itself guarantee that new consumers can
bootstrap from cached codec configuration + keyframe.

Choose the smallest clean solution.

---

# HLS source switching

The fix also needs to behave correctly when Program changes source.

Potential source changes include:

- hard cut
- preview → program
- scene activation
- failover
- failback/recovery

Sequence headers may change when switching sources.

ChannelHls must not remain permanently dependent on headers captured at HLS startup.

Investigate whether the programme router already sends fresh sequence headers on
source switch.

If yes, ensure ChannelHls correctly receives/uses them.

If no, improve the programme path so downstream consumers get a coherent FLV stream:

    new codec headers
    -> keyframe
    -> subsequent packets

---

# Instrumentation

Add useful logs around HLS bootstrap.

For example:

    channel HLS: subscribing to program stream
    channel HLS: effective program source <uuid>
    channel HLS: video sequence header available: yes/no
    channel HLS: audio sequence header available: yes/no
    channel HLS: waiting for first keyframe
    channel HLS: received first keyframe after X ms
    channel HLS: first playlist created after X ms

Do not log every media packet.

These logs should make this class of failure diagnosable.

---

# HLS acceptance tests

## Start from idle

1. Guest/source live.
2. Public Channel enabled.
3. Press Go Live.
4. Confirm:

       /config/hls/<token>/index.m3u8

   appears within ~6 seconds.

5. Confirm segments appear.
6. Confirm `/watch/<token>` changes to ON AIR automatically.

## Enable Channel while already broadcasting

1. Go Live with Public Channel disabled.
2. Wait at least 30 seconds.
3. Enable Public Channel.
4. Playlist must appear within ~6 seconds.

This is the scenario that previously sometimes took several minutes.

## Restart Channel output

While broadcasting:

    Public Channel OFF
    wait
    Public Channel ON

HLS must restart cleanly within seconds.

## Source switch

While HLS is running:

    Source A → Source B

Public watch page must continue playing.

No multi-minute stall.

## Scene switch

Switch:

    direct guest → scene → direct guest

HLS should continue.

## Failover

If practical, verify HLS survives:

    intended source
       ↓ failure
    fallback
       ↓ recovery
    intended source

---

# Issue 2 — Scene compositor outputs silence

## Observed process

The running scene compositor ffmpeg command looked effectively like:

    ffmpeg
      -f flv -i tcp://127.0.0.1:<port>
      -f lavfi -i anullsrc=r=48000:cl=stereo
      -filter_complex <video composition>
      -map [video-output]
      -map 1:a
      ...
      -c:a aac
      ...

This means scene audio is explicitly:

    anullsrc

i.e. silence.

So currently:

    Guest 1 audio ------X
    Guest 2 audio ------X

    Guest video(s)
          |
          v
     scene compositor
          |
          +---- composed video
          |
          +---- synthetic silence

That is not suitable for StreamYard-style operation.

---

# Desired audio model

Muxshed already has global/program audio routing concepts.

A scene must not automatically destroy source audio.

At minimum, a scene should support:

    Audio Follows Video

where appropriate.

For a basic first implementation, when a scene is on Program:

- retain the audio source selected by Muxshed's audio routing
- do not replace it with `anullsrc`

A scene's visual compositor does NOT necessarily need to become the audio mixer.

It is acceptable — and probably preferable — to keep:

    Scene compositor = video composition

and have:

    Program router = audio selection

provided the final programme output combines them correctly.

---

# Architectural preference

Avoid baking a complete multichannel audio mixer into every scene ffmpeg command
unless genuinely necessary.

Preferred model:

    VIDEO:
        multiple scene inputs
              |
              v
        scene compositor
              |
              v
        scene video relay

    AUDIO:
        selected audio source
              |
              v
        program router

    FINAL PROGRAM:
        scene video + routed audio

This fits Muxshed's existing separation between:

    program video source

and:

    audio routing

better than hardwiring a scene to synthetic silence.

---

# Current program router behaviour

The program router already conceptually handles:

    video source = current program source

and:

    audio source =
        current video source
        OR explicitly selected source

It can subscribe separately to a different audio relay when required.

Make scenes work with that architecture rather than bypassing it.

---

# Scene audio cases to support

## One guest scene

Scene containing Guest A.

With Audio Follows Video:

    video = scene composition
    audio = Guest A

## Two-person scene

Scene contains:

    Guest A
    Guest B

For first implementation, it is acceptable to use one explicitly selected audio source.

Longer term we may want both participants mixed.

Do not fake multichannel mixing if it is not implemented yet.

## Independent audio source

Operator should be able to select:

    video = scene
    audio = Guest B

and get Guest B audio.

## Muted source

If audio routing says a source is muted, programme output should respect it.

---

# Future audio mixer direction

This does NOT have to be completed in this task, but keep the implementation compatible with future:

- multiple simultaneously audible guests
- per-source mute
- per-source level
- per-source gain
- mix-minus
- PFL/solo
- real level meters

Avoid architecture that assumes "exactly one audio source forever".

---

# Important related producer UI issue

Current Muxshed Studio audio meters are cosmetic.

They use random values such as:

    Math.random()

rather than actual measured signal level.

Do not use these meters as evidence that audio exists.

Our separate fork work may replace them with real Web Audio meters.

---

# Acceptance tests — audio

## Direct guest

Guest microphone → Program directly.

Expected:

    video works
    audio works

## Scene with one guest

Put Guest A into a scene.

Take scene to Program.

Expected:

    scene video visible
    Guest A microphone audible

No `anullsrc` silence in programme output.

## Manual audio source

With scene on Program:

Select Guest B as active audio source.

Expected:

    scene video remains
    Guest B audio becomes programme audio

## Audio follows video

Switch direct Program between guests.

Expected:

    audio follows selected direct source

Switch to a scene.

Expected behaviour should be explicitly defined and predictable.

## Public HLS

Verify the public Channel output contains:

    H.264 video
    AAC stereo audio

Use ffprobe if useful:

    ffprobe <HLS playlist>

Confirm audio stream is present and not synthetic silence when a real guest is selected.

---

# Issue 3 — stale GStreamer text

The Channel UI currently says:

    "Video only plays while you are streaming.
     The public HLS output requires the GStreamer / Docker build of Muxshed."

This is stale.

Current Muxshed HLS implementation uses ffmpeg and the Docker image already bundles it.

Change this text.

Suggested wording:

    "Video plays while the studio is on air.
     The public channel is generated as HLS by Muxshed's ffmpeg media pipeline."

Or simply remove the implementation-specific sentence.

---

# Do not expose extra ports for this

The Public Channel does NOT require:

    1935/TCP
    9000/UDP

to be publicly forwarded.

Those are ingest ports for:

    RTMP
    SRT

The public watch page and HLS playlist/segments are served through the normal
Muxshed HTTP/API service behind HTTPS.

Do not change networking as part of this fix.

---

# Useful production observations

Current running processes included:

## Guest WebRTC ingest normaliser

    ffmpeg ...
    -i /config/ingest-<uuid>.sdp
    ...
    -c:v libx264
    -c:a aac
    -f flv pipe:1

This was consuming significant CPU and working.

## Scene compositor

    ffmpeg ...
    -i tcp://127.0.0.1:<port>
    -f lavfi -i anullsrc...
    ...
    -f flv pipe:1

Also consuming significant CPU.

## Channel HLS

    ffmpeg
    -f flv -i pipe:0
    ...
    -f hls
    -hls_time 2
    ...
    /config/hls/<token>/index.m3u8

Process started successfully but initially produced no files.

Eventually it began producing:

    seg00000.ts
    seg00001.ts
    ...
    index.m3u8

This proves ffmpeg installation, codecs, filesystem permissions and HLS output
directory are fundamentally working.

Focus investigation on media bootstrap/flow, not Docker ports.

---

# Deliverables

Please produce:

1. Fix for deterministic/fast Channel HLS startup.
2. Appropriate tests around late HLS subscription.
3. Logging around first headers/keyframe/playlist generation.
4. Fix for scene/program audio so scenes do not force silence.
5. Tests for scene video + selected source audio.
6. Replace/remove stale GStreamer wording.
7. Update `FORK_NOTES.md`.
8. Brief explanation of root cause(s).

Avoid broad unrelated refactoring.

If a defect in upstream Muxshed is clearly identified, keep the changes clean
enough that they could potentially be proposed upstream later.