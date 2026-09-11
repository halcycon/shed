// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

import { FilesetResolver, ImageSegmenter } from '@mediapipe/tasks-vision';
import type { BackgroundProcessor } from './background-processor';
import { DEFAULT_BLUR_PX, PROCESS_HEIGHT, SEGMENT_HEIGHT } from './background-processor';

const WASM_BASE = '/mediapipe';
const MODEL_PATH = '/mediapipe/selfie_segmenter.tflite';

type VideoFrameCallback = (now: number, metadata: unknown) => void;

/**
 * MediaPipe ImageSegmenter-backed blur processor.
 * Composites a sharp person over a blurred full frame onto a canvas stream.
 */
export class MediapipeBlurProcessor implements BackgroundProcessor {
	private segmenter: ImageSegmenter | null = null;
	private source: HTMLVideoElement | null = null;
	private canvas: HTMLCanvasElement;
	private ctx: CanvasRenderingContext2D;
	private maskCanvas: HTMLCanvasElement;
	private maskCtx: CanvasRenderingContext2D;
	private blurCanvas: HTMLCanvasElement;
	private blurCtx: CanvasRenderingContext2D;
	private stream: MediaStream | null = null;
	private running = false;
	private destroyed = false;
	private useRvfc = false;
	private rafId = 0;
	private lastTimestamp = -1;
	private blurPx: number;
	private frameCount = 0;
	private fpsWindowStart = 0;
	private maskImageData: ImageData | null = null;

	constructor(blurPx = DEFAULT_BLUR_PX) {
		this.blurPx = blurPx;
		this.canvas = document.createElement('canvas');
		const ctx = this.canvas.getContext('2d', { alpha: false });
		if (!ctx) throw new Error('2D canvas unavailable');
		this.ctx = ctx;

		this.maskCanvas = document.createElement('canvas');
		const maskCtx = this.maskCanvas.getContext('2d', { willReadFrequently: true });
		if (!maskCtx) throw new Error('2D canvas unavailable');
		this.maskCtx = maskCtx;

		this.blurCanvas = document.createElement('canvas');
		const blurCtx = this.blurCanvas.getContext('2d', { alpha: false });
		if (!blurCtx) throw new Error('2D canvas unavailable');
		this.blurCtx = blurCtx;
	}

	async initialise(): Promise<void> {
		if (this.destroyed) throw new Error('processor destroyed');
		const vision = await FilesetResolver.forVisionTasks(WASM_BASE);
		try {
			this.segmenter = await ImageSegmenter.createFromOptions(vision, {
				baseOptions: {
					modelAssetPath: MODEL_PATH,
					delegate: 'GPU'
				},
				runningMode: 'VIDEO',
				outputCategoryMask: false,
				outputConfidenceMasks: true
			});
		} catch {
			// GPU delegate can fail on some Linux setups — fall back to CPU.
			this.segmenter = await ImageSegmenter.createFromOptions(vision, {
				baseOptions: {
					modelAssetPath: MODEL_PATH,
					delegate: 'CPU'
				},
				runningMode: 'VIDEO',
				outputCategoryMask: false,
				outputConfidenceMasks: true
			});
		}
		this.stream = this.canvas.captureStream(30);
	}

	setSource(video: HTMLVideoElement): void {
		this.source = video;
		this.lastTimestamp = -1;
	}

	start(): void {
		if (this.running || this.destroyed || !this.segmenter) return;
		this.running = true;
		this.fpsWindowStart = performance.now();
		this.frameCount = 0;
		this.useRvfc = typeof this.source?.requestVideoFrameCallback === 'function';
		if (this.useRvfc && this.source) {
			this.source.requestVideoFrameCallback(this.onVideoFrame);
		} else {
			this.rafId = requestAnimationFrame(this.onRaf);
		}
	}

	stop(): void {
		this.running = false;
		cancelAnimationFrame(this.rafId);
		this.rafId = 0;
	}

	getTrack(): MediaStreamTrack | null {
		return this.stream?.getVideoTracks()[0] ?? null;
	}

	getStream(): MediaStream | null {
		return this.stream;
	}

	destroy(): void {
		this.destroyed = true;
		this.stop();
		this.segmenter?.close();
		this.segmenter = null;
		this.stream?.getTracks().forEach((t) => t.stop());
		this.stream = null;
		this.source = null;
	}

	private onVideoFrame: VideoFrameCallback = (_now, _meta) => {
		if (!this.running || !this.source) return;
		this.renderFrame();
		if (this.running && this.source) {
			this.source.requestVideoFrameCallback(this.onVideoFrame);
		}
	};

	private onRaf = () => {
		if (!this.running) return;
		this.renderFrame();
		if (this.running) this.rafId = requestAnimationFrame(this.onRaf);
	};

	private renderFrame(): void {
		const video = this.source;
		const segmenter = this.segmenter;
		if (!video || !segmenter || video.readyState < 2) return;

		const vw = video.videoWidth || 0;
		const vh = video.videoHeight || 0;
		if (vw < 2 || vh < 2) return;

		const outH = Math.min(PROCESS_HEIGHT, vh);
		const outW = Math.max(2, Math.round((vw / vh) * outH));
		if (this.canvas.width !== outW || this.canvas.height !== outH) {
			this.canvas.width = outW;
			this.canvas.height = outH;
			this.blurCanvas.width = outW;
			this.blurCanvas.height = outH;
		}

		const segH = Math.min(SEGMENT_HEIGHT, outH);
		const segW = Math.max(2, Math.round((outW / outH) * segH));
		if (this.maskCanvas.width !== segW || this.maskCanvas.height !== segH) {
			this.maskCanvas.width = segW;
			this.maskCanvas.height = segH;
		}

		// Blurred background
		this.blurCtx.filter = `blur(${this.blurPx}px)`;
		this.blurCtx.drawImage(video, 0, 0, outW, outH);
		this.blurCtx.filter = 'none';

		const ts = performance.now();
		// MediaPipe requires strictly increasing timestamps in VIDEO mode.
		const stamp = ts <= this.lastTimestamp ? this.lastTimestamp + 1 : ts;
		this.lastTimestamp = stamp;

		try {
			segmenter.segmentForVideo(video, stamp, (result) => {
				if (this.destroyed || !this.running) return;
				const mask = result.confidenceMasks?.[0];
				if (!mask) {
					// Fail soft: send unblurred frame rather than freezing.
					this.ctx.drawImage(video, 0, 0, outW, outH);
					return;
				}

				const mw = mask.width;
				const mh = mask.height;
				if (this.maskCanvas.width !== mw || this.maskCanvas.height !== mh) {
					this.maskCanvas.width = mw;
					this.maskCanvas.height = mh;
					this.maskImageData = null;
				}

				if (!this.maskImageData || this.maskImageData.width !== mw || this.maskImageData.height !== mh) {
					this.maskImageData = this.maskCtx.createImageData(mw, mh);
				}
				const imageData = this.maskImageData;
				const data = imageData.data;
				// Confidence mask: higher = more likely person.
				const conf = mask.getAsFloat32Array();
				for (let i = 0; i < conf.length; i++) {
					const person = conf[i]! > 0.5 ? 255 : 0;
					const o = i * 4;
					data[o] = 255;
					data[o + 1] = 255;
					data[o + 2] = 255;
					data[o + 3] = person;
				}
				this.maskCtx.putImageData(imageData, 0, 0);
				mask.close();

				// Draw blurred bg, then sharp person using mask as alpha.
				this.ctx.globalCompositeOperation = 'copy';
				this.ctx.drawImage(this.blurCanvas, 0, 0, outW, outH);
				this.ctx.globalCompositeOperation = 'destination-out';
				this.ctx.drawImage(this.maskCanvas, 0, 0, outW, outH);
				this.ctx.globalCompositeOperation = 'destination-over';
				this.ctx.drawImage(video, 0, 0, outW, outH);
				this.ctx.globalCompositeOperation = 'source-over';

				this.frameCount++;
				const elapsed = performance.now() - this.fpsWindowStart;
				if (elapsed >= 2000) {
					const fps = (this.frameCount * 1000) / elapsed;
					if (import.meta.env.DEV) {
						console.debug(`[guestux] blur processing ~${fps.toFixed(1)} fps @ ${outW}x${outH}`);
					}
					this.frameCount = 0;
					this.fpsWindowStart = performance.now();
				}
			});
		} catch (e) {
			console.warn('[guestux] segmentation frame failed', e);
			this.ctx.drawImage(video, 0, 0, outW, outH);
		}
	}
}

/** Try to create and initialise a blur processor; returns null on failure. */
export async function tryCreateBlurProcessor(): Promise<MediapipeBlurProcessor | null> {
	try {
		const p = new MediapipeBlurProcessor();
		await p.initialise();
		return p;
	} catch (e) {
		console.warn('[guestux] background effects unavailable', e);
		return null;
	}
}
