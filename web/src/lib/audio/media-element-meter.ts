// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

/**
 * Local level meter for an HTMLMediaElement (studio preview monitoring).
 *
 * Routes element audio through Web Audio so we can measure RMS even while muted.
 * Connects to AudioContext.destination only when monitoring is enabled.
 * Never feeds a microphone into this graph — use headphones to avoid feedback.
 */

export type MediaElementMeterHandle = {
	getLevel: () => number;
	setMonitoring: (enabled: boolean) => void;
	destroy: () => void;
};

export function createMediaElementMeter(el: HTMLMediaElement): MediaElementMeterHandle {
	const ctx = new AudioContext();
	const source = ctx.createMediaElementSource(el);
	const analyser = ctx.createAnalyser();
	analyser.fftSize = 512;
	analyser.smoothingTimeConstant = 0.7;
	const gain = ctx.createGain();
	gain.gain.value = 0;

	source.connect(analyser);
	analyser.connect(gain);
	gain.connect(ctx.destination);

	const buf = new Float32Array(analyser.fftSize);
	let level = 0;
	let raf = 0;
	let alive = true;

	const tick = () => {
		if (!alive) return;
		analyser.getFloatTimeDomainData(buf);
		let sum = 0;
		for (let i = 0; i < buf.length; i++) {
			const v = buf[i]!;
			sum += v * v;
		}
		const rms = Math.sqrt(sum / buf.length);
		const next = Math.min(1, rms * 4.5);
		level = level * 0.7 + next * 0.3;
		raf = requestAnimationFrame(tick);
	};
	raf = requestAnimationFrame(tick);
	void ctx.resume().catch(() => {});

	return {
		getLevel: () => level,
		setMonitoring: (enabled: boolean) => {
			gain.gain.value = enabled ? 1 : 0;
			if (enabled) void ctx.resume().catch(() => {});
		},
		destroy: () => {
			alive = false;
			cancelAnimationFrame(raf);
			try {
				source.disconnect();
				analyser.disconnect();
				gain.disconnect();
			} catch {
				/* already disconnected */
			}
			void ctx.close().catch(() => {});
		}
	};
}
