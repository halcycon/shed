<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { api } from '$lib/api';
	import { connectWs, disconnectWs } from '$lib/ws';
	import { sources } from '$lib/stores/pipeline';
	import type { AudioRouting, Source } from '$lib/types';

	let programId = $state<string | null>(null);
	let audioRouting = $state<AudioRouting>({ active_audio_source: null, channels: [], audio_follows_video: true });
	let channel: BroadcastChannel;

	function liveSources(): Source[] {
		return $sources.filter((s) => s.state === 'live');
	}

	onMount(async () => {
		connectWs();
		sources.set(await api.listSources());
		audioRouting = await api.getAudioRouting();

		channel = new BroadcastChannel('muxshed-studio');
		channel.onmessage = (e) => {
			if (e.data.type === 'program_source') programId = e.data.sourceId;
			if (e.data.type === 'audio_routing') audioRouting = e.data.routing;
		};
		channel.postMessage({ type: 'request_state' });
	});

	onDestroy(() => {
		disconnectWs();
		channel?.close();
	});

	async function setAudioSource(id: string | null) {
		await api.setAudioSource(id);
		audioRouting.active_audio_source = id;
		audioRouting.audio_follows_video = id === null;
		channel?.postMessage({ type: 'audio_routing', routing: audioRouting });
	}

	async function toggleFollows() {
		await api.toggleAudioFollowsVideo();
		audioRouting.audio_follows_video = !audioRouting.audio_follows_video;
		if (audioRouting.audio_follows_video) audioRouting.active_audio_source = null;
		channel?.postMessage({ type: 'audio_routing', routing: audioRouting });
	}
	function meterBarHeight(level: number, index: number, bars = 10): number {
		const threshold = (index + 1) / bars;
		if (level >= threshold) return 100;
		if (level >= threshold - 1 / bars) {
			return Math.max(12, Math.round(((level - (threshold - 1 / bars)) * bars) * 100));
		}
		return 12;
	}

	// Popout has no Program preview — show a flat active indicator, never Math.random().
	let activeLevel = $derived(0);
</script>

<svelte:head><title>Audio - Muxshed</title></svelte:head>

<section class="panel">
	<header class="panel__head">
		<span>▮ AUDIO MIXER</span>
		<button
			onclick={toggleFollows}
			class="btn {audioRouting.audio_follows_video ? 'btn--go' : ''}"
			style="min-height:24px;padding:2px 8px"
		>
			{audioRouting.audio_follows_video ? 'Follows Video' : 'Independent'}
		</button>
	</header>
	<div class="panel__body space-y-2">
		<p class="text-[11px] text-amber-muted">
			Levels update on the main Studio Program monitor. This popout shows routing only.
		</p>
		{#each liveSources() as source (source.id)}
			{@const isActive = audioRouting.audio_follows_video ? source.id === programId : source.id === audioRouting.active_audio_source}
			<button
				onclick={() => setAudioSource(source.id)}
				disabled={audioRouting.audio_follows_video}
				class="row w-full text-left {isActive ? 'border-live' : ''} disabled:cursor-default"
			>
				<div class="scanlines-well flex h-6 w-16 items-end gap-px border border-border-dim p-px">
					{#each Array(10) as _, i}
						{@const h = isActive ? meterBarHeight(activeLevel, i) : 12}
						<div
							class="w-1 {isActive ? (i < 7 ? 'bg-live' : i < 9 ? 'bg-warning' : 'bg-danger') : 'bg-border-dim'}"
							style="height: {h}%"
						></div>
					{/each}
				</div>
				<span class="flex-1 {isActive ? 'text-amber-bright' : 'text-amber-dim'}">{source.name}</span>
				{#if isActive}
					<span class="pill pill--live">● ACTIVE</span>
				{/if}
			</button>
		{/each}
	</div>
</section>
