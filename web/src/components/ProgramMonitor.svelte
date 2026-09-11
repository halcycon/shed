<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	/** Program bus WHEP monitor (thin wrapper). */
	import WhepMonitor from './WhepMonitor.svelte';

	let {
		fallbackSourceId = null as string | null,
		monitorAudio = false,
		audioLevel = $bindable(0),
		active = true
	}: {
		fallbackSourceId?: string | null;
		monitorAudio?: boolean;
		audioLevel?: number;
		active?: boolean;
	} = $props();
</script>

<WhepMonitor
	target={{ kind: 'program' }}
	{fallbackSourceId}
	{monitorAudio}
	bind:audioLevel
	{active}
/>
