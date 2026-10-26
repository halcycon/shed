# MediaPipe assets (guest background blur)

Served from our own origin so guest browsers never depend on a third-party CDN at runtime.
Loaded only when a guest first turns on Blur.

| Asset | Source | Licence |
|-------|--------|---------|
| `wasm/` (vision tasks runtime) | Copied from `@mediapipe/tasks-vision` in `node_modules` by `scripts/copy-mediapipe-wasm.mjs` (`predev` / `prebuild`); gitignored | Apache-2.0 |
| `selfie_segmenter.tflite` | MediaPipe Image Segmenter (selfie) float16, committed | Apache-2.0 |

See https://developers.google.com/mediapipe and the package privacy notice.
