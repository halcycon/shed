// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

/** Background effect modes supported by the guest green room. */
export type EffectsMode = 'none' | 'blur';

/**
 * Local (in-browser) video background processor.
 *
 * Implementations must not upload frames to any remote inference service.
 */
export interface BackgroundProcessor {
	initialise(): Promise<void>;
	/** Attach or replace the camera source used for segmentation. */
	setSource(video: HTMLVideoElement): void;
	/** Start the render/segmentation loop (idempotent). */
	start(): void;
	/** Stop the loop without destroying the processor (tracks may still exist). */
	stop(): void;
	/** Outgoing canvas-captured video track. */
	getTrack(): MediaStreamTrack | null;
	getStream(): MediaStream | null;
	destroy(): void;
}

export const DEFAULT_BLUR_PX = 12;
/** Target processed output height (width follows source aspect). */
export const PROCESS_HEIGHT = 720;
/** Segmentation mask height (lower = cheaper). */
export const SEGMENT_HEIGHT = 256;
