<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	/** Program bus monitor — tuned WS-FLV (no WHEP re-encode). */
	import VideoPreview from './VideoPreview.svelte';

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

{#if fallbackSourceId}
	<VideoPreview
		sourceId={fallbackSourceId}
		profile="monitor"
		{monitorAudio}
		bind:audioLevel
		{active}
	/>
{:else}
	<div class="scanlines-well flex aspect-video items-center justify-center border border-border">
		<span class="text-amber-muted">No source</span>
	</div>
{/if}
