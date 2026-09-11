<!-- Licensed under the GNU Affero General Public License v3.0 — see LICENSE. -->
<script lang="ts">
	/**
	 * Programme audio mixer strips: AFV / Independent / Mix, mute, volume, PFL,
	 * and analyse → suggest → apply DSP (jive-inspired, on-bus).
	 */
	import VideoPreview from './VideoPreview.svelte';
	import PopoutButton from './PopoutButton.svelte';
	import { api } from '$lib/api';
	import type { AudioDspFilters, AudioRouting, Source } from '$lib/types';

	let {
		sources,
		programSourceId = null as string | null,
		audioRouting = $bindable({
			active_audio_source: null,
			channels: [],
			audio_follows_video: true,
			mix_live_sources: false
		} as AudioRouting),
		programAudioLevel = 0,
		/** Hide when already inside the audio popout window. */
		showPopout = true,
		onrouting
	}: {
		sources: Source[];
		programSourceId?: string | null;
		audioRouting: AudioRouting;
		programAudioLevel?: number;
		showPopout?: boolean;
		onrouting?: (r: AudioRouting) => void;
	} = $props();

	let pflSourceId = $state<string | null>(null);
	let error = $state('');
	let analysingId = $state<string | null>(null);
	/** Pending suggestions keyed by source id (not yet applied). */
	let pendingSuggest = $state<Record<string, AudioDspFilters>>({});

	const mixMode = $derived(!!audioRouting.mix_live_sources);

	function liveSources(): Source[] {
		return sources.filter((s) => s.state === 'live' && s.kind.type !== 'media_file');
	}

	function emptyFilters(): AudioDspFilters {
		return {
			enabled: false,
			highpass_hz: 0,
			denoise: false,
			gate: false,
			compress: false,
			notes: []
		};
	}

	function channel(id: string) {
		return (
			audioRouting.channels.find((c) => c.source_id === id) ?? {
				source_id: id,
				muted: false,
				volume: 1,
				filters: emptyFilters()
			}
		);
	}

	function filtersActive(f?: AudioDspFilters): boolean {
		if (!f?.enabled) return false;
		return f.highpass_hz > 1 || f.denoise || f.gate || f.compress;
	}

	function filterSummary(f: AudioDspFilters): string {
		const bits: string[] = [];
		if (f.highpass_hz > 1) bits.push(`HP${Math.round(f.highpass_hz)}`);
		if (f.denoise) bits.push('DN');
		if (f.gate) bits.push('GT');
		if (f.compress) bits.push('CMP');
		return bits.join('+') || 'DSP';
	}

	function isOnAirAudio(id: string): boolean {
		if (mixMode) {
			const ch = channel(id);
			return !ch.muted && ch.volume > 0.001;
		}
		if (audioRouting.audio_follows_video) return id === programSourceId;
		return id === audioRouting.active_audio_source;
	}

	function publish(r: AudioRouting) {
		audioRouting = r;
		onrouting?.(r);
	}

	async function setMode(mode: 'afv' | 'independent' | 'mix') {
		error = '';
		try {
			let r = await api.getAudioRouting();
			if (mode === 'mix') {
				if (!r.mix_live_sources) r = await api.toggleAudioMix();
				publish(r);
				return;
			}
			if (r.mix_live_sources) {
				r = await api.toggleAudioMix();
			}
			if (mode === 'afv') {
				if (!r.audio_follows_video) {
					await api.toggleAudioFollowsVideo();
					r = await api.getAudioRouting();
				}
			} else {
				const id = r.active_audio_source ?? programSourceId;
				if (id) {
					await api.setAudioSource(id);
					r = await api.getAudioRouting();
				} else if (r.audio_follows_video) {
					await api.toggleAudioFollowsVideo();
					r = await api.getAudioRouting();
				}
			}
			publish(r);
		} catch (e) {
			error = String(e);
		}
	}

	async function selectIndependent(id: string) {
		if (mixMode || audioRouting.audio_follows_video) return;
		try {
			await api.setAudioSource(id);
			publish({
				...audioRouting,
				active_audio_source: id,
				audio_follows_video: false,
				mix_live_sources: false
			});
		} catch (e) {
			error = String(e);
		}
	}

	async function toggleMute(id: string) {
		const ch = channel(id);
		try {
			if (ch.muted) await api.unmuteSource(id);
			else await api.muteSource(id);
			const channels = [...audioRouting.channels];
			const idx = channels.findIndex((c) => c.source_id === id);
			const next = { ...ch, muted: !ch.muted };
			if (idx >= 0) channels[idx] = next;
			else channels.push(next);
			publish({ ...audioRouting, channels });
		} catch (e) {
			error = String(e);
		}
	}

	async function setVolume(id: string, volume: number) {
		try {
			await api.setSourceVolume(id, volume);
			const channels = [...audioRouting.channels];
			const idx = channels.findIndex((c) => c.source_id === id);
			const ch = { ...channel(id), volume };
			if (idx >= 0) channels[idx] = ch;
			else channels.push(ch);
			publish({ ...audioRouting, channels });
		} catch (e) {
			error = String(e);
		}
	}

	async function analyse(id: string) {
		error = '';
		analysingId = id;
		try {
			const result = await api.analyseSourceAudio(id);
			pendingSuggest = { ...pendingSuggest, [id]: result.suggestion };
		} catch (e) {
			error = String(e);
		} finally {
			analysingId = null;
		}
	}

	async function applyFilters(id: string, filters: AudioDspFilters) {
		error = '';
		try {
			const r = await api.setSourceFilters(id, { ...filters, enabled: true });
			const next = { ...pendingSuggest };
			delete next[id];
			pendingSuggest = next;
			publish(r);
		} catch (e) {
			error = String(e);
		}
	}

	async function clearFilters(id: string) {
		error = '';
		try {
			const r = await api.setSourceFilters(id, emptyFilters());
			const next = { ...pendingSuggest };
			delete next[id];
			pendingSuggest = next;
			publish(r);
		} catch (e) {
			error = String(e);
		}
	}

	function togglePfl(id: string) {
		pflSourceId = pflSourceId === id ? null : id;
	}

	function meterBarHeight(level: number, index: number, bars = 8): number {
		const threshold = (index + 1) / bars;
		if (level >= threshold) return 100;
		if (level >= threshold - 1 / bars) {
			return Math.max(12, Math.round(((level - (threshold - 1 / bars)) * bars) * 100));
		}
		return 12;
	}
</script>

<section class="panel">
	<header class="panel__head">
		<span class="flex items-center gap-2">
			▮ AUDIO
			{#if showPopout}
				<PopoutButton section="audio" width={480} height={640} />
			{/if}
		</span>
		<div class="flex gap-1">
			<button
				type="button"
				class="btn {audioRouting.audio_follows_video && !mixMode ? 'btn--go' : ''}"
				style="min-height:24px;padding:2px 8px"
				onclick={() => setMode('afv')}
			>
				Follows Video
			</button>
			<button
				type="button"
				class="btn {!audioRouting.audio_follows_video && !mixMode ? 'btn--go' : ''}"
				style="min-height:24px;padding:2px 8px"
				onclick={() => setMode('independent')}
			>
				Independent
			</button>
			<button
				type="button"
				class="btn {mixMode ? 'btn--go' : ''}"
				style="min-height:24px;padding:2px 8px"
				title="Mix all unmuted live sources into programme audio (stays on the Muxshed bus)."
				onclick={() => setMode('mix')}
			>
				Mix
			</button>
		</div>
	</header>
	<div class="panel__body space-y-3">
		<p class="text-[11px] text-amber-muted">
			{#if mixMode}
				Mixing unmuted live sources into Program. Cut video freely — lips stay with each contribution’s bus timing.
			{:else if audioRouting.audio_follows_video}
				Audio follows Program video. Use mute/volume or switch to Mix for multi-mic.
			{:else}
				Independent: one source feeds programme audio. Pick a strip below.
			{/if}
			PFL is headphones-only (not on air). Analyse samples ~12s then suggests bus DSP — Apply puts it on air.
		</p>
		{#if error}
			<p class="text-[11px] text-danger">{error}</p>
		{/if}

		{#each liveSources() as source (source.id)}
			{@const ch = channel(source.id)}
			{@const onAir = isOnAirAudio(source.id)}
			{@const pfl = pflSourceId === source.id}
			{@const applied = ch.filters}
			{@const suggest = pendingSuggest[source.id]}
			<div class="row flex-col items-stretch gap-2 {onAir ? 'border-live' : ''}">
				<div class="flex flex-wrap items-center gap-2">
					<div class="scanlines-well flex h-8 w-14 shrink-0 items-end gap-px border border-border-dim p-px">
						{#each Array(8) as _, i}
							{@const h = onAir ? meterBarHeight(programAudioLevel, i) : 12}
							<div
								class="w-1 {onAir ? (i < 6 ? 'bg-live' : i < 7 ? 'bg-warning' : 'bg-danger') : 'bg-border-dim'}"
								style="height: {h}%"
							></div>
						{/each}
					</div>
					<button
						type="button"
						class="min-w-0 flex-1 truncate text-left {onAir ? 'text-amber-bright' : 'text-amber-dim'}"
						disabled={mixMode || audioRouting.audio_follows_video}
						onclick={() => selectIndependent(source.id)}
					>
						{source.name}
					</button>
					{#if onAir}
						<span class="pill pill--live shrink-0">● PGM</span>
					{/if}
					{#if filtersActive(applied)}
						<span class="pill shrink-0" title={(applied?.notes ?? []).join(' ')}>{filterSummary(applied!)}</span>
					{/if}
					<button
						type="button"
						class="btn shrink-0 {ch.muted ? 'btn--danger' : ''}"
						style="min-height:24px;padding:2px 8px"
						onclick={() => toggleMute(source.id)}
					>
						{ch.muted ? 'Muted' : 'Mute'}
					</button>
					<label class="flex shrink-0 items-center gap-1 text-[10px] text-amber-dim">
						Vol
						<input
							type="range"
							min="0"
							max="1.5"
							step="0.05"
							value={ch.volume}
							class="w-20 accent-[var(--color-amber)]"
							oninput={(e) => setVolume(source.id, Number((e.currentTarget as HTMLInputElement).value))}
						/>
						<span class="w-8 tabular-nums text-amber">{Math.round(ch.volume * 100)}</span>
					</label>
					<button
						type="button"
						class="btn shrink-0 {pfl ? 'btn--go' : ''}"
						style="min-height:24px;padding:2px 8px"
						title="Pre-fade listen — local only"
						onclick={() => togglePfl(source.id)}
					>
						PFL
					</button>
					<button
						type="button"
						class="btn shrink-0"
						style="min-height:24px;padding:2px 8px"
						disabled={analysingId === source.id}
						title="Capture ~12s and suggest spoken-word DSP (not applied until you confirm)"
						onclick={() => analyse(source.id)}
					>
						{analysingId === source.id ? '…' : 'Analyse'}
					</button>
					{#if filtersActive(applied)}
						<button
							type="button"
							class="btn shrink-0"
							style="min-height:24px;padding:2px 8px"
							onclick={() => clearFilters(source.id)}
						>
							Clear DSP
						</button>
					{/if}
				</div>
				{#if suggest}
					<div class="rounded border border-border-dim bg-surface-dim/40 px-2 py-1.5 text-[10px] text-amber-muted">
						<p class="mb-1 text-amber">Suggested: {filterSummary(suggest)}</p>
						{#each suggest.notes ?? [] as note}
							<p>{note}</p>
						{/each}
						<div class="mt-1.5 flex gap-1">
							<button
								type="button"
								class="btn btn--go"
								style="min-height:24px;padding:2px 8px"
								onclick={() => applyFilters(source.id, suggest)}
							>
								Apply
							</button>
							<button
								type="button"
								class="btn"
								style="min-height:24px;padding:2px 8px"
								onclick={() => {
									const next = { ...pendingSuggest };
									delete next[source.id];
									pendingSuggest = next;
								}}
							>
								Dismiss
							</button>
						</div>
					</div>
				{/if}
			</div>
		{/each}

		{#if pflSourceId}
			<div class="sr-only" aria-hidden="true">
				<VideoPreview sourceId={pflSourceId} profile="monitor" latencyHint="none" monitorAudio={true} />
			</div>
			<p class="text-[10px] text-amber-muted">PFL: {liveSources().find((s) => s.id === pflSourceId)?.name}</p>
		{/if}
	</div>
</section>
