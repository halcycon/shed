// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

/**
 * Local microphone level meter via Web Audio AnalyserNode.
 * Never connects to AudioContext.destination (no local monitoring / echo).
 */

export type MicMeterHandle = {
	/** 0–1 smoothed level */
	getLevel: () => number;
	destroy: () => void;
};

export function createMicMeter(track: MediaStreamTrack): MicMeterHandle {
	const stream = new MediaStream([track]);
	const ctx = new AudioContext();
	const source = ctx.createMediaStreamSource(stream);
	const analyser = ctx.createAnalyser();
	analyser.fftSize = 512;
	analyser.smoothingTimeConstant = 0.7;
	source.connect(analyser);

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
		// Soft knee so speech is visible without pegging on peaks.
		const next = Math.min(1, rms * 4.5);
		level = level * 0.7 + next * 0.3;
		raf = requestAnimationFrame(tick);
	};
	raf = requestAnimationFrame(tick);

	// Resume if the browser started the context suspended.
	void ctx.resume().catch(() => {});

	return {
		getLevel: () => (track.enabled ? level : 0),
		destroy: () => {
			alive = false;
			cancelAnimationFrame(raf);
			try {
				source.disconnect();
			} catch {
				/* already disconnected */
			}
			void ctx.close().catch(() => {});
		}
	};
}
