// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

// Copy the MediaPipe vision WASM runtime out of node_modules into static/ so
// guest background blur is served from our own origin without committing it.
// Runs from the predev / prebuild npm hooks; the output folder is gitignored.

import { cpSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const wasmDir = dirname(require.resolve('@mediapipe/tasks-vision/vision_wasm_internal.wasm'));
const dest = fileURLToPath(new URL('../static/mediapipe/wasm', import.meta.url));

cpSync(wasmDir, dest, { recursive: true });
