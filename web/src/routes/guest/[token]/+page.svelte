<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { page } from '$app/stores';
	import { createMicMeter, type MicMeterHandle } from '$lib/audio/mic-meter';
	import type { EffectsMode } from '$lib/video-effects/background-processor';
	import { tryCreateBlurProcessor, type MediapipeBlurProcessor } from '$lib/video-effects/mediapipe-segmenter';

	const token = $derived($page.params.token);

	type Mode =
		| 'loading'
		| 'invalid'
		| 'ready'
		| 'joining'
		| 'live'
		| 'reconnecting'
		| 'pending'
		| 'error';
	let mode = $state<Mode>('loading');
	let guestName = $state('');
	let message = $state('');
	let effectsWarning = $state('');

	// Channel branding (Studio → Channel)
	let channelTitle = $state('');
	let channelLogoUrl = $state<string | null>(null);
	let channelAccent = $state<string | null>(null);

	let videoEl = $state<HTMLVideoElement | null>(null);
	/** Hidden element feeding the blur processor (never mirrored). */
	let processVideoEl = $state<HTMLVideoElement | null>(null);

	let iceServers: RTCIceServer[] = [];
	let pc: RTCPeerConnection | null = null;

	// Explicit media state (SPEC)
	let rawStream = $state<MediaStream | null>(null);
	let rawVideoTrack = $state<MediaStreamTrack | null>(null);
	let microphoneTrack = $state<MediaStreamTrack | null>(null);
	let effectsMode = $state<EffectsMode>('none');
	let processor: MediapipeBlurProcessor | null = null;
	let processedStream = $state<MediaStream | null>(null);
	let processedVideoTrack = $state<MediaStreamTrack | null>(null);
	let cameraEnabled = $state(true);
	let microphoneEnabled = $state(true);
	let blurAvailable = $state(false);

	let cameras = $state<MediaDeviceInfo[]>([]);
	let mics = $state<MediaDeviceInfo[]>([]);
	let camId = $state('');
	let micId = $state('');

	let micMeter: MicMeterHandle | null = null;
	let micLevel = $state(0);
	let meterRaf = 0;

	const brandLabel = $derived(channelTitle.trim() || 'Guest');
	const accent = $derived(channelAccent || 'var(--color-amber)');
	const busy = $derived(mode === 'joining');

	/** What the guest is transmitting (and should preview). */
	const previewStream = $derived.by(() => {
		if (effectsMode === 'blur' && processedStream) return processedStream;
		return rawStream;
	});

	$effect(() => {
		if (videoEl && previewStream) videoEl.srcObject = previewStream;
	});

	$effect(() => {
		if (processVideoEl && rawStream) processVideoEl.srcObject = rawStream;
	});

	onMount(() => {
		window.addEventListener('online', onBrowserOnline);
		void init();
	});
	onDestroy(() => {
		wantLive = false;
		clearReconnectTimers();
		if (typeof window !== 'undefined') window.removeEventListener('online', onBrowserOnline);
		teardownMedia();
		pc?.close();
		pc = null;
	});

	async function init() {
		mode = 'loading';
		message = '';
		effectsWarning = '';
		try {
			const res = await fetch(`/api/v1/guest/${token}`);
			if (!res.ok) {
				mode = 'invalid';
				return;
			}
			const info = await res.json();
			guestName = info.name ?? 'Guest';
			channelTitle = info.channel_title ?? '';
			channelLogoUrl = info.channel_logo_url ?? null;
			channelAccent = info.channel_accent ?? null;
			if (Array.isArray(info.ice_servers) && info.ice_servers.length > 0) {
				iceServers = info.ice_servers;
			}

			await acquireDevices();
			await refreshDevices();
			startMicMeter();

			// Warm blur processor in the background — failure must not block joining.
			void ensureBlurReady();

			mode = 'ready';
		} catch (e) {
			message = e instanceof Error ? e.message : 'Could not access camera or microphone.';
			mode = 'error';
		}
	}

	async function ensureBlurReady(): Promise<boolean> {
		if (processor) {
			blurAvailable = true;
			return true;
		}
		const created = await tryCreateBlurProcessor();
		if (!created) {
			blurAvailable = false;
			effectsWarning = 'Background effects are not available in this browser.';
			return false;
		}
		processor = created;
		blurAvailable = true;
		return true;
	}

	async function acquireDevices() {
		const prevVideo = rawVideoTrack;
		const prevAudio = microphoneTrack;

		// Without a size hint Chrome captures 640x480, which the studio has to
		// upscale and pillarbox onto its 16:9 canvas.
		const video: MediaTrackConstraints = {
			width: { ideal: 1280 },
			height: { ideal: 720 },
			frameRate: { ideal: 30, max: 30 }
		};
		const stream = await navigator.mediaDevices.getUserMedia({
			video: camId ? { ...video, deviceId: { exact: camId } } : video,
			audio: micId ? { deviceId: { exact: micId } } : true
		});

		prevVideo?.stop();
		prevAudio?.stop();

		rawStream = stream;
		rawVideoTrack = stream.getVideoTracks()[0] ?? null;
		microphoneTrack = stream.getAudioTracks()[0] ?? null;
		camId = rawVideoTrack?.getSettings().deviceId ?? camId;
		micId = microphoneTrack?.getSettings().deviceId ?? micId;

		if (rawVideoTrack) rawVideoTrack.enabled = cameraEnabled;
		if (microphoneTrack) microphoneTrack.enabled = microphoneEnabled;

		if (import.meta.env.DEV && rawVideoTrack) {
			const s = rawVideoTrack.getSettings();
			console.debug('[guestux] camera', s.width, s.height, s.frameRate, rawVideoTrack.id);
		}
	}

	async function refreshDevices() {
		const devs = await navigator.mediaDevices.enumerateDevices();
		cameras = devs.filter((d) => d.kind === 'videoinput');
		mics = devs.filter((d) => d.kind === 'audioinput');
	}

	function startMicMeter() {
		stopMicMeter();
		if (!microphoneTrack) return;
		micMeter = createMicMeter(microphoneTrack);
		const tick = () => {
			micLevel = micMeter?.getLevel() ?? 0;
			meterRaf = requestAnimationFrame(tick);
		};
		meterRaf = requestAnimationFrame(tick);
	}

	function stopMicMeter() {
		cancelAnimationFrame(meterRaf);
		meterRaf = 0;
		micMeter?.destroy();
		micMeter = null;
		micLevel = 0;
	}

	function outboundVideoTrack(): MediaStreamTrack | null {
		if (effectsMode === 'blur' && processedVideoTrack) return processedVideoTrack;
		return rawVideoTrack;
	}

	async function replaceOutgoingVideoTrack(track: MediaStreamTrack | null) {
		if (!pc || !track) return;
		const videoSender = pc.getSenders().find((s) => s.track?.kind === 'video');
		if (videoSender) await videoSender.replaceTrack(track);
		if (import.meta.env.DEV) {
			console.debug('[guestux] outbound video track', track.id);
		}
	}

	async function replaceOutgoingAudioTrack(track: MediaStreamTrack | null) {
		if (!pc || !track) return;
		const audioSender = pc.getSenders().find((s) => s.track?.kind === 'audio');
		if (audioSender) await audioSender.replaceTrack(track);
	}

	async function rebuildVideoPipeline() {
		if (effectsMode !== 'blur') {
			stopProcessor();
			await replaceOutgoingVideoTrack(rawVideoTrack);
			return;
		}

		const ok = await ensureBlurReady();
		if (!ok || !processor || !processVideoEl || !rawVideoTrack) {
			effectsMode = 'none';
			await replaceOutgoingVideoTrack(rawVideoTrack);
			return;
		}

		// Ensure the hidden source element is playing before segmentation.
		processVideoEl.srcObject = rawStream;
		await processVideoEl.play().catch(() => {});

		processor.setSource(processVideoEl);
		processor.start();
		processedStream = processor.getStream();
		processedVideoTrack = processor.getTrack();
		if (processedVideoTrack) {
			processedVideoTrack.enabled = cameraEnabled;
		}
		await replaceOutgoingVideoTrack(processedVideoTrack);
	}

	function stopProcessor() {
		processor?.stop();
		// Keep the processor instance warm; only clear processed track refs.
		processedVideoTrack = null;
		processedStream = null;
	}

	async function setBackgroundMode(next: EffectsMode) {
		if (next === effectsMode) return;
		if (next === 'blur' && !blurAvailable) {
			const ok = await ensureBlurReady();
			if (!ok) return;
		}
		effectsMode = next;
		await rebuildVideoPipeline();
	}

	function setCameraEnabled(enabled: boolean) {
		cameraEnabled = enabled;
		if (rawVideoTrack) rawVideoTrack.enabled = enabled;
		if (processedVideoTrack) processedVideoTrack.enabled = enabled;
	}

	function setMicrophoneEnabled(enabled: boolean) {
		microphoneEnabled = enabled;
		if (microphoneTrack) microphoneTrack.enabled = enabled;
	}

	async function switchDevice() {
		try {
			const wasBlur = effectsMode === 'blur';
			if (wasBlur) stopProcessor();

			await acquireDevices();
			await refreshDevices();
			startMicMeter();

			if (pc) {
				await replaceOutgoingAudioTrack(microphoneTrack);
			}

			if (wasBlur) {
				await rebuildVideoPipeline();
			} else if (pc) {
				await replaceOutgoingVideoTrack(rawVideoTrack);
			}
		} catch (e) {
			message = e instanceof Error ? e.message : 'Could not switch device.';
		}
	}

	function teardownMedia() {
		stopMicMeter();
		processor?.destroy();
		processor = null;
		processedStream = null;
		processedVideoTrack = null;
		rawStream?.getTracks().forEach((t) => t.stop());
		rawStream = null;
		rawVideoTrack = null;
		microphoneTrack = null;
		blurAvailable = false;
	}

	function waitForIceGathering(peer: RTCPeerConnection, timeoutMs = 3000): Promise<void> {
		if (peer.iceGatheringState === 'complete') return Promise.resolve();
		return new Promise((resolve) => {
			const finish = () => {
				peer.removeEventListener('icegatheringstatechange', check);
				clearTimeout(timer);
				resolve();
			};
			const check = () => {
				if (peer.iceGatheringState === 'complete') finish();
			};
			const timer = setTimeout(finish, timeoutMs);
			peer.addEventListener('icegatheringstatechange', check);
		});
	}

	/** True from "Join" until the guest leaves; drives automatic reconnects. */
	let wantLive = false;
	let reconnectAttempt = $state(0);
	let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	let unstableTimer: ReturnType<typeof setTimeout> | null = null;
	/** How long "disconnected" may last before we rebuild the connection. */
	const UNSTABLE_MS = 5000;
	const MAX_RETRY_MS = 30000;

	function clearReconnectTimers() {
		if (reconnectTimer) clearTimeout(reconnectTimer);
		if (unstableTimer) clearTimeout(unstableTimer);
		reconnectTimer = null;
		unstableTimer = null;
	}

	function scheduleReconnect() {
		if (!wantLive) return;
		clearReconnectTimers();
		pc?.close();
		pc = null;
		mode = 'reconnecting';
		const delay = Math.min(2000 * 2 ** reconnectAttempt, MAX_RETRY_MS);
		reconnectAttempt += 1;
		reconnectTimer = setTimeout(() => void join(true), delay);
	}

	function onBrowserOnline() {
		if (mode !== 'reconnecting') return;
		clearReconnectTimers();
		void join(true);
	}

	async function join(retry = false) {
		const videoTrack = outboundVideoTrack();
		if (!videoTrack || !microphoneTrack) return;
		if (!retry) {
			wantLive = true;
			reconnectAttempt = 0;
			mode = 'joining';
		}
		message = '';
		const peer = new RTCPeerConnection({ iceServers });
		pc = peer;
		try {
			peer.addTrack(videoTrack, new MediaStream([videoTrack]));
			peer.addTrack(microphoneTrack, new MediaStream([microphoneTrack]));

			peer.onconnectionstatechange = () => {
				if (peer !== pc) return;
				const s = peer.connectionState;
				if (s === 'connected') {
					clearReconnectTimers();
					reconnectAttempt = 0;
					mode = 'live';
				} else if (s === 'disconnected' && wantLive) {
					// Often a brief blip that recovers by itself; rebuild only if it lasts.
					mode = 'reconnecting';
					if (!unstableTimer) {
						unstableTimer = setTimeout(() => {
							unstableTimer = null;
							if (peer === pc && peer.connectionState !== 'connected') scheduleReconnect();
						}, UNSTABLE_MS);
					}
				} else if (s === 'failed') {
					if (wantLive) {
						scheduleReconnect();
					} else {
						message = 'Connection failed — check your network, or set a TURN server.';
						mode = 'error';
					}
				}
			};

			await peer.setLocalDescription(await peer.createOffer());
			await waitForIceGathering(peer);
			if (peer !== pc) return;

			if (import.meta.env.DEV && peer.localDescription?.sdp) {
				const hasOpus = /opus\/48000/i.test(peer.localDescription.sdp);
				console.debug('[guestux] offer contains Opus:', hasOpus);
			}

			const res = await fetch(`/api/v1/guest/${token}/whip`, {
				method: 'POST',
				headers: { 'Content-Type': 'application/sdp' },
				body: peer.localDescription?.sdp ?? ''
			});
			if (peer !== pc) return;

			if (res.status === 503) {
				wantLive = false;
				peer.close();
				pc = null;
				mode = 'pending';
				return;
			}
			if (res.status === 404) {
				wantLive = false;
				peer.close();
				pc = null;
				mode = 'invalid';
				return;
			}
			if (!res.ok) throw new Error(`server returned ${res.status}`);

			const answer = await res.text();
			if (import.meta.env.DEV) {
				const m = answer.match(/a=rtpmap:(\d+) opus\/48000/i);
				console.debug('[guestux] answer Opus PT:', m?.[1] ?? 'not found');
			}
			await peer.setRemoteDescription({ type: 'answer', sdp: answer });
		} catch (e) {
			if (peer !== pc) return;
			if (wantLive && retry) {
				scheduleReconnect();
				return;
			}
			wantLive = false;
			message = e instanceof Error ? e.message : 'Could not connect.';
			mode = 'error';
		}
	}

	function disconnect() {
		wantLive = false;
		clearReconnectTimers();
		pc?.close();
		pc = null;
		mode = 'ready';
		message = '';
	}

	const meterWidth = $derived(Math.round(micLevel * 100));
</script>

<svelte:head>
	<title>{brandLabel} — Join as guest</title>
</svelte:head>

<!-- Hidden source for the blur processor (unmirrored). -->
<!-- svelte-ignore a11y_media_has_caption -->
<video
	bind:this={processVideoEl}
	class="pointer-events-none fixed h-px w-px opacity-0"
	autoplay
	playsinline
	muted
></video>

<div class="flex min-h-screen flex-col items-center justify-center p-4">
	{#if mode === 'loading'}
		<span class="label animate-pulse">Loading…</span>
	{:else if mode === 'invalid'}
		<div class="panel w-full max-w-md text-center">
			<div class="panel__body py-10">
				<p class="mb-2 text-lg tracking-widest" style="color: {accent}">◉ {brandLabel}</p>
				<p class="text-sm text-amber-dim">This guest link is invalid or has expired.</p>
			</div>
		</div>
	{:else}
		<div class="w-full max-w-2xl space-y-4">
			<header class="flex items-center justify-between gap-3">
				<div class="flex min-w-0 items-center gap-3">
					{#if channelLogoUrl}
						<img src={channelLogoUrl} alt="" class="h-8 w-8 rounded object-contain" />
					{/if}
					<span class="truncate text-lg tracking-widest glow" style="color: {accent}">
						◉ {brandLabel}
						<span class="text-amber-muted">· GUEST</span>
					</span>
				</div>
				{#if mode === 'live'}
					<span class="pill pill--live"><span class="tally">●</span> ON AIR</span>
				{:else if mode === 'joining'}
					<span class="pill pill--idle animate-pulse">○ connecting</span>
				{:else if mode === 'reconnecting'}
					<span class="pill pill--warning animate-pulse">○ reconnecting</span>
				{:else}
					<span class="pill pill--idle">○ {guestName}</span>
				{/if}
			</header>

			<div
				class="scanlines-well relative w-full overflow-hidden rounded-md border border-border"
				style="aspect-ratio: 16 / 9"
			>
				<!-- svelte-ignore a11y_media_has_caption -->
				<video
					bind:this={videoEl}
					class="h-full w-full bg-black"
					class:opacity-40={!cameraEnabled}
					style="transform: scaleX(-1)"
					autoplay
					playsinline
					muted
				></video>
				{#if !cameraEnabled}
					<div class="absolute inset-0 flex items-center justify-center text-sm text-amber-muted">
						Camera off
					</div>
				{/if}
			</div>

			<div class="grid gap-2 sm:grid-cols-2">
				<label class="block">
					<span class="field-label">Camera</span>
					<select class="input" bind:value={camId} onchange={switchDevice}>
						{#each cameras as c, i}
							<option value={c.deviceId}>{c.label || `Camera ${i + 1}`}</option>
						{/each}
					</select>
				</label>
				<label class="block">
					<span class="field-label">Microphone</span>
					<select class="input" bind:value={micId} onchange={switchDevice}>
						{#each mics as m, i}
							<option value={m.deviceId}>{m.label || `Microphone ${i + 1}`}</option>
						{/each}
					</select>
				</label>
			</div>

			<div class="flex flex-wrap items-center gap-2">
				<span class="field-label mb-0">Background</span>
				<button
					type="button"
					class="btn"
					class:btn--go={effectsMode === 'none'}
					onclick={() => setBackgroundMode('none')}
				>
					None
				</button>
				<button
					type="button"
					class="btn"
					class:btn--go={effectsMode === 'blur'}
					onclick={() => setBackgroundMode('blur')}
				>
					Blur
				</button>
				{#if effectsWarning}
					<span class="text-[12px] text-amber-muted">{effectsWarning}</span>
				{/if}
			</div>

			<div class="grid gap-2 sm:grid-cols-2">
				<button
					type="button"
					class="btn w-full"
					onclick={() => setCameraEnabled(!cameraEnabled)}
				>
					Camera: {cameraEnabled ? 'On' : 'Off'}
				</button>
				<button
					type="button"
					class="btn w-full"
					onclick={() => setMicrophoneEnabled(!microphoneEnabled)}
				>
					Mic: {microphoneEnabled ? 'Unmuted' : 'Muted'}
				</button>
			</div>

			<div>
				<span class="field-label">Mic level</span>
				<div class="mt-1 h-2 w-full overflow-hidden rounded bg-black/40">
					<div
						class="h-full transition-[width] duration-75"
						class:opacity-40={!microphoneEnabled}
						style="width: {meterWidth}%; background: {accent}"
					></div>
				</div>
			</div>

			{#if mode === 'ready'}
				<button class="btn btn--go w-full" onclick={() => join()}>● Join the broadcast</button>
				<p class="text-center text-[12px] text-amber-muted">
					Pick your camera and mic above, then join when you're ready.
				</p>
			{:else if mode === 'joining'}
				<p class="text-center text-sm text-amber-dim animate-pulse">Connecting…</p>
				<button class="btn w-full" onclick={disconnect}>Cancel</button>
			{:else if mode === 'live'}
				<p class="text-center text-sm text-live-glow">● You are live in the studio.</p>
				<button class="btn btn--danger w-full" onclick={disconnect}>Disconnect</button>
			{:else if mode === 'reconnecting'}
				<p class="text-center text-sm text-warning animate-pulse">
					Connection lost — reconnecting{reconnectAttempt > 1 ? ` (attempt ${reconnectAttempt})` : ''}…
				</p>
				<p class="text-center text-[12px] text-amber-muted">
					Stay on this page; you'll rejoin the studio automatically.
				</p>
				<button class="btn w-full" onclick={disconnect}>Leave</button>
			{:else if mode === 'pending'}
				<div class="panel">
					<div class="panel__body text-center">
						<p class="text-sm text-warning">Guest video ingest isn't enabled on this instance yet.</p>
						<p class="mt-1 text-[12px] text-amber-muted">Your link is valid — the host can see you're here.</p>
					</div>
				</div>
			{:else if mode === 'error'}
				<p class="text-center text-sm text-danger-glow">{message}</p>
				<button class="btn w-full" disabled={busy} onclick={() => (pc ? join() : init())}>Try again</button>
			{/if}
		</div>
	{/if}
</div>
