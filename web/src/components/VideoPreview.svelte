<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import mpegts from 'mpegts.js';
	import {
		createMediaElementMeter,
		type MediaElementMeterHandle
	} from '$lib/audio/media-element-meter';

	let {
		sourceId,
		label = '',
		active = false,
		/** When true, preview audio plays locally (user-gesture toggle). Default muted. */
		monitorAudio = false,
		/** Optional bindable 0–1 level from the preview's AnalyserNode. */
		audioLevel = $bindable(0),
		onclick
	}: {
		sourceId: string;
		label?: string;
		active?: boolean;
		monitorAudio?: boolean;
		audioLevel?: number;
		onclick?: () => void;
	} = $props();

	let videoEl: HTMLVideoElement;
	let player: mpegts.Player | null = null;
	let destroyed = false;
	let meter: MediaElementMeterHandle | null = null;
	let meterRaf = 0;

	$effect(() => {
		meter?.setMonitoring(monitorAudio);
		// Keep the element muted attribute in sync as a safety net; actual
		// audible path is the Web Audio gain node.
		if (videoEl) videoEl.muted = !monitorAudio;
	});

	function createPlayer() {
		if (!mpegts.isSupported() || destroyed) return;

		const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
		const url = `${proto}//${window.location.host}/api/v1/sources/${sourceId}/preview`;

		player = mpegts.createPlayer(
			{
				type: 'flv',
				isLive: true,
				url
			},
			{
				enableWorker: false,
				liveBufferLatencyChasing: true,
				liveBufferLatencyMaxLatency: 1.5,
				liveBufferLatencyMinRemain: 0.3
			}
		);

		player.on(mpegts.Events.ERROR, () => {
			destroyPlayer();
			if (!destroyed) {
				setTimeout(createPlayer, 2000);
			}
		});

		player.attachMediaElement(videoEl);
		player.load();

		videoEl.onloadeddata = () => {
			if (!destroyed && videoEl) {
				videoEl.play().catch(() => {});
				ensureMeter();
			}
		};
	}

	function ensureMeter() {
		if (meter || !videoEl) return;
		try {
			meter = createMediaElementMeter(videoEl);
			meter.setMonitoring(monitorAudio);
			const tick = () => {
				audioLevel = meter?.getLevel() ?? 0;
				meterRaf = requestAnimationFrame(tick);
			};
			meterRaf = requestAnimationFrame(tick);
		} catch (e) {
			console.warn('[preview] audio meter unavailable', e);
		}
	}

	function destroyPlayer() {
		cancelAnimationFrame(meterRaf);
		meterRaf = 0;
		meter?.destroy();
		meter = null;
		audioLevel = 0;
		if (player) {
			try {
				player.unload();
				player.detachMediaElement();
				player.destroy();
			} catch {
				// ignore cleanup errors
			}
			player = null;
		}
	}

	onMount(() => {
		createPlayer();
	});

	onDestroy(() => {
		destroyed = true;
		destroyPlayer();
	});
</script>

<button
	class="scanlines-well group relative aspect-video w-full overflow-hidden rounded-md border {active
		? 'border-live'
		: 'border-border hover:border-amber-dim'}"
	onclick={onclick}
>
	<video bind:this={videoEl} class="h-full w-full object-contain" muted playsinline></video>
	{#if label}
		<div
			class="absolute bottom-0 left-0 right-0 border-t border-border-dim bg-panel-raised px-3 py-1.5"
		>
			<span class="label">{label}</span>
		</div>
	{/if}
	{#if active}
		<div class="pill pill--live absolute top-2 right-2">
			● LIVE
		</div>
	{/if}
</button>
