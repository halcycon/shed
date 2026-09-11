<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	/**
	 * WS-FLV preview. Grid uses a safer buffer; Preview/Program use a chased
	 * monitor profile (still FLV — no server re-encode / WHEP).
	 */
	import { onDestroy } from 'svelte';
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
		/**
		 * grid — conservative buffer, ~2–5s cue
		 * monitor — aggressive chase for Preview/Program switching
		 */
		profile = 'grid' as 'grid' | 'monitor',
		/** Override badge; default follows profile. */
		latencyHint = undefined as 'delayed' | 'monitor' | 'none' | undefined,
		onclick
	}: {
		sourceId: string;
		label?: string;
		active?: boolean;
		monitorAudio?: boolean;
		audioLevel?: number;
		profile?: 'grid' | 'monitor';
		latencyHint?: 'delayed' | 'monitor' | 'none';
		onclick?: () => void;
	} = $props();

	let videoEl = $state<HTMLVideoElement | null>(null);
	let player: mpegts.Player | null = null;
	let destroyed = false;
	let meter: MediaElementMeterHandle | null = null;
	let meterRaf = 0;

	const hint = $derived(
		latencyHint ?? (profile === 'monitor' ? 'monitor' : 'delayed')
	);

	$effect(() => {
		meter?.setMonitoring(monitorAudio);
		if (videoEl) videoEl.muted = !monitorAudio;
	});

	// Recreate the player when source or profile changes (no full component remount).
	$effect(() => {
		const id = sourceId;
		const prof = profile;
		const el = videoEl;
		if (!el || destroyed) return;

		destroyPlayer();
		createPlayer(id, prof, el);

		return () => {
			destroyPlayer();
		};
	});

	function bufferOpts(prof: 'grid' | 'monitor') {
		if (prof === 'monitor') {
			return {
				enableWorker: false as const,
				liveBufferLatencyChasing: true,
				liveBufferLatencyMaxLatency: 0.7,
				liveBufferLatencyMinRemain: 0.12
			};
		}
		return {
			enableWorker: false as const,
			liveBufferLatencyChasing: true,
			liveBufferLatencyMaxLatency: 1.5,
			liveBufferLatencyMinRemain: 0.3
		};
	}

	function createPlayer(id: string, prof: 'grid' | 'monitor', el: HTMLVideoElement) {
		if (!mpegts.isSupported() || destroyed) return;

		const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
		const url = `${proto}//${window.location.host}/api/v1/sources/${id}/preview`;

		player = mpegts.createPlayer(
			{
				type: 'flv',
				isLive: true,
				url
			},
			bufferOpts(prof)
		);

		player.on(mpegts.Events.ERROR, () => {
			destroyPlayer();
			if (!destroyed) {
				setTimeout(() => {
					if (!destroyed && videoEl) createPlayer(id, prof, videoEl);
				}, 1500);
			}
		});

		player.attachMediaElement(el);
		player.load();

		el.onloadeddata = () => {
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
	{#if hint === 'delayed'}
		<div
			class="absolute top-2 left-2 rounded-sm border border-border-dim bg-panel-raised/90 px-1.5 py-0.5 text-[9px] uppercase tracking-wide text-amber-muted"
			title="Grid tiles use a safer FLV buffer. Use Preview/Program (tuned FLV) when switching."
		>
			~2–5s
		</div>
	{:else if hint === 'monitor'}
		<div
			class="absolute top-2 left-2 rounded-sm border border-border-dim bg-panel-raised/90 px-1.5 py-0.5 text-[9px] uppercase tracking-wide text-amber"
			title="Tuned FLV monitor (no WebRTC re-encode). Still slightly behind the contribution."
		>
			Monitor FLV
		</div>
	{/if}
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
