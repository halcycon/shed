<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	/**
	 * Low-latency monitor for Program or a single source via WHEP,
	 * with WS-FLV fallback. Thumbnails should keep using VideoPreview (FLV).
	 */
	import { onMount, onDestroy } from 'svelte';
	import VideoPreview from './VideoPreview.svelte';
	import { connectWhep, type WhepHandle, type WhepTarget } from '$lib/whep-player';
	import {
		createMediaElementMeter,
		type MediaElementMeterHandle
	} from '$lib/audio/media-element-meter';

	let {
		/** Prefer WHEP for this feed. */
		target,
		/** Source id for WS-FLV fallback (required for source feeds; Program uses current program source). */
		fallbackSourceId = null as string | null,
		monitorAudio = false,
		audioLevel = $bindable(0),
		active = true
	}: {
		target: WhepTarget;
		fallbackSourceId?: string | null;
		monitorAudio?: boolean;
		audioLevel?: number;
		active?: boolean;
	} = $props();

	type Mode = 'connecting' | 'whep' | 'flv';
	let mode = $state<Mode>('connecting');
	let videoEl = $state<HTMLVideoElement | null>(null);
	let whep = $state<WhepHandle | null>(null);
	let meter: MediaElementMeterHandle | null = null;
	let meterRaf = 0;
	let destroyed = false;
	let statusNote = $state('');

	$effect(() => {
		if (mode === 'whep' && videoEl && whep) {
			videoEl.srcObject = whep.stream;
			void videoEl.play().catch(() => {});
			ensureMeter();
		}
	});

	$effect(() => {
		meter?.setMonitoring(monitorAudio);
		if (videoEl) videoEl.muted = !monitorAudio;
	});

	onMount(() => {
		void start();
	});

	onDestroy(() => {
		destroyed = true;
		teardown();
	});

	async function start() {
		mode = 'connecting';
		statusNote = 'Connecting WebRTC…';
		try {
			whep = await connectWhep(target);
			if (destroyed) {
				await whep.close();
				return;
			}
			mode = 'whep';
			statusNote = 'WebRTC';
			if (videoEl) {
				videoEl.srcObject = whep.stream;
				await videoEl.play().catch(() => {});
				ensureMeter();
			}
		} catch (e) {
			console.warn('[whep-monitor] WHEP unavailable, falling back to WS-FLV', e);
			statusNote = 'WS-FLV fallback';
			mode = 'flv';
		}
	}

	function ensureMeter() {
		if (meter || !videoEl || mode !== 'whep') return;
		try {
			meter = createMediaElementMeter(videoEl);
			meter.setMonitoring(monitorAudio);
			const tick = () => {
				audioLevel = meter?.getLevel() ?? 0;
				meterRaf = requestAnimationFrame(tick);
			};
			meterRaf = requestAnimationFrame(tick);
		} catch (e) {
			console.warn('[whep-monitor] meter unavailable', e);
		}
	}

	function teardown() {
		cancelAnimationFrame(meterRaf);
		meterRaf = 0;
		meter?.destroy();
		meter = null;
		audioLevel = 0;
		void whep?.close();
		whep = null;
	}
</script>

<div class="relative">
	{#if mode === 'flv'}
		{#if fallbackSourceId}
			{#key fallbackSourceId}
				<VideoPreview
					sourceId={fallbackSourceId}
					{active}
					{monitorAudio}
					bind:audioLevel
				/>
			{/key}
		{:else}
			<div class="scanlines-well flex aspect-video items-center justify-center border border-border">
				<span class="text-amber-muted">No source</span>
			</div>
		{/if}
	{:else}
		<div
			class="scanlines-well relative aspect-video w-full overflow-hidden rounded-md border {active
				? 'border-live'
				: 'border-border'}"
		>
			<!-- svelte-ignore a11y_media_has_caption -->
			<video bind:this={videoEl} class="h-full w-full object-contain bg-black" playsinline muted
			></video>
			{#if mode === 'connecting'}
				<div class="absolute inset-0 flex items-center justify-center text-sm text-amber-muted">
					Connecting low-latency preview…
				</div>
			{/if}
			{#if active}
				<div class="pill pill--live absolute top-2 right-2">● LIVE</div>
			{/if}
		</div>
	{/if}
	<p class="border-t border-border-dim px-3 py-1 text-[10px] text-amber-muted">{statusNote}</p>
</div>
