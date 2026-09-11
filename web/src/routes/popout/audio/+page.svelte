<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { api } from '$lib/api';
	import { connectWs, disconnectWs } from '$lib/ws';
	import { sources } from '$lib/stores/pipeline';
	import AudioMixerPanel from '../../../components/AudioMixerPanel.svelte';
	import type { AudioRouting } from '$lib/types';

	let programId = $state<string | null>(null);
	let audioRouting = $state<AudioRouting>({
		active_audio_source: null,
		channels: [],
		audio_follows_video: true,
		mix_live_sources: false
	});
	let channel: BroadcastChannel;

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
</script>

<svelte:head><title>Audio - Muxshed</title></svelte:head>

<div class="p-2">
	<AudioMixerPanel
		sources={$sources}
		programSourceId={programId}
		bind:audioRouting
		programAudioLevel={0}
		showPopout={false}
		onrouting={(r) => {
			audioRouting = r;
			channel?.postMessage({ type: 'audio_routing', routing: r });
		}}
	/>
</div>
