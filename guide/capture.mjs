// Regenerates the screenshots in guide/img against a throwaway local instance.
//
//   1. Build the web UI (cd web && npm run build) and the API (cargo build -p muxshed-api).
//   2. Start a fresh instance on 127.0.0.1:18080 with RTMP on 11935, e.g.
//        MUXSHED_LISTEN_ADDR=127.0.0.1:18080 MUXSHED_RTMP_PORT=11935 \
//        MUXSHED_DB_PATH=/tmp/mxdocs/muxshed.db MUXSHED_DATA_DIR=/tmp/mxdocs \
//        MUXSHED_WEB_DIR=$PWD/web/build ./target/debug/muxshed-api
//   3. Copy this file to a scratch dir, install playwright-core there and run it:
//        cp guide/capture.mjs /tmp/shots/ && cd /tmp/shots && npm i playwright-core
//        GUIDE_IMG=<repo>/guide/img CHROMIUM=/usr/bin/chromium-browser node capture.mjs
//
// Needs ffmpeg (test-pattern feeds). The database must be empty: the script runs first-time setup.
// The load endpoint is stubbed so the shots don't depend on how busy the capturing machine is.

import { chromium } from 'playwright-core';
import { spawn } from 'child_process';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const BASE = 'http://studio.example.com';
const IMG = process.env.GUIDE_IMG || path.join(path.dirname(fileURLToPath(import.meta.url)), 'img');
fs.mkdirSync(IMG, { recursive: true });

const browser = await chromium.launch({
	executablePath: process.env.CHROMIUM || '/usr/bin/chromium-browser',
	args: [
		'--host-resolver-rules=MAP studio.example.com:80 127.0.0.1:18080',
		'--unsafely-treat-insecure-origin-as-secure=http://studio.example.com',
		'--use-fake-device-for-media-stream',
		'--use-fake-ui-for-media-stream',
		'--autoplay-policy=no-user-gesture-required',
	],
});
const viewport = { width: 1440, height: 900 };
const ctx = await browser.newContext({ viewport, permissions: ['camera', 'microphone'] });
const page = await ctx.newPage();
const wait = (ms) => page.waitForTimeout(ms);
const shot = (p, name, opts = {}) => p.screenshot({ path: `${IMG}/${name}.png`, ...opts });
const el = (p, loc, name, pad = 12) =>
	loc.boundingBox().then((b) =>
		p.screenshot({
			path: `${IMG}/${name}.png`,
			clip: { x: Math.max(0, b.x - pad), y: Math.max(0, b.y - pad), width: b.width + pad * 2, height: b.height + pad * 2 },
		}),
	);
const api = (method, path, data) =>
	page.evaluate(
		async ([m, p, d]) => {
			const headers = { Authorization: `Bearer ${localStorage.getItem('muxshed_session_token')}` };
			if (d) headers['Content-Type'] = 'application/json';
			const r = await fetch('/api/v1' + p, { method: m, headers, body: d ? JSON.stringify(d) : undefined });
			if (!r.ok) throw new Error(`${m} ${p}: ${r.status} ${await r.text()}`);
			return r.status === 204 ? null : r.json();
		},
		[method, path, data],
	);
const procs = [];
process.on('exit', () => procs.forEach((p) => p.kill('SIGKILL')));

const calmLoad = (route) =>
	route.fulfill({ json: { cpu_percent: 41, cpu_cores: 8, overloaded: false, stages: [] } });
await page.route('**/api/v1/status/load', calmLoad);

// ---- First run + login
await page.goto(BASE + '/');
await wait(1500);
await page.fill('#setup-name', 'My Studio');
await shot(page, '01-setup');
await page.click('button[type=submit]');
await page.waitForSelector('text=Continue to login', { timeout: 30000 });
await wait(500);
await page.screenshot({ path: `${IMG}/02-setup-done.png`, mask: [page.getByText(/^mxs_/)] });
await page.click('text=Continue to login');
await wait(800);
await page.fill('#login-user', 'admin');
await page.fill('#login-pass', 'admin');
await shot(page, '03-login');
await page.click('button[type=submit]');
await wait(2500);
{
	const cfg = await api('GET', '/output/config');
	await api('PUT', '/output/config', { ...cfg, width: 1280, height: 720, video_bitrate_kbps: 3000, webrtc_ingest_preset: 'ultrafast' });
	await page.reload();
	await wait(2500);
}
await shot(page, '04-studio-empty');

// ---- Sources
await page.goto(BASE + '/sources');
await wait(1200);
const addSource = async (protocol, name) => {
	await page.selectOption('#source-protocol', protocol);
	await page.fill('#source-name', name);
	await page.click('button[type=submit]');
	await wait(1000);
};
await page.fill('#source-name', 'Presenter camera');
await el(page, page.locator('section.panel').first(), '05-add-source');
await page.click('button[type=submit]');
await wait(1000);
await addSource('rtmp', 'Slides laptop');
await addSource('webrtc', 'OBS via WHIP');
await addSource('srt', 'Field camera');
await page.reload();
await wait(1500);
const card = (name) =>
	page.locator('div.rounded-md.border', { has: page.getByText(name, { exact: true }) }).last();
await el(page, card('Presenter camera'), '06-source-rtmp-card', 6);
await el(page, card('OBS via WHIP'), '07-source-whip-card', 6);
await el(page, card('Field camera'), '08-source-srt-card', 6);

// ---- Feed the RTMP sources with test patterns
const sources = await api('GET', '/sources');
const feed = (key, label, pattern) => {
	const p = spawn(
		'ffmpeg',
		[
			'-loglevel', 'error', '-re',
			'-f', 'lavfi', '-i', `${pattern}=size=1280x720:rate=30`,
			'-f', 'lavfi', '-i', 'sine=frequency=330:sample_rate=48000',
			'-vf', `drawtext=text='${label}':fontsize=64:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=20:x=(w-tw)/2:y=h-th-60`,
			'-c:v', 'libx264', '-preset', 'ultrafast', '-tune', 'zerolatency', '-g', '60', '-pix_fmt', 'yuv420p',
			'-c:a', 'aac', '-b:a', '128k',
			'-f', 'flv', `rtmp://127.0.0.1:11935/live/${key}`,
		],
		{ stdio: ['ignore', 'ignore', 'inherit'] },
	);
	procs.push(p);
};
for (const s of sources) {
	if (s.kind.type !== 'rtmp') continue;
	if (s.name === 'Presenter camera') feed(s.kind.stream_key, 'PRESENTER CAMERA', 'testsrc2');
	if (s.name === 'Slides laptop') feed(s.kind.stream_key, 'SLIDES', 'smptehdbars');
}

// ---- Guests
await page.goto(BASE + '/guests');
await wait(1200);
for (const g of ['Jordan', 'Sam']) {
	await page.fill('#guest-name', g);
	await page.click('button[type=submit]');
	await wait(1000);
}
await page.fill('#guest-name', '');
await el(page, page.locator('section.panel').nth(1), '09-guests-invited');
const guests = await api('GET', '/guests');

const guestPages = [];
for (const g of guests) {
	const gctx = await browser.newContext({ viewport: { width: 1280, height: 860 }, permissions: ['camera', 'microphone'] });
	const gp = await gctx.newPage();
	await gp.goto(`${BASE}/guest/${g.token}`);
	await gp.waitForTimeout(4000);
	if (g.name === 'Jordan') await shot(gp, '10-guest-green-room', { fullPage: true });
	await gp.click('text=Join the broadcast');
	guestPages.push([g.name, gp]);
}
await wait(8000);
for (const [name, gp] of guestPages) {
	if (name === 'Jordan') await shot(gp, '11-guest-live', { fullPage: true });
}

// ---- Studio with live sources
await page.goto(BASE + '/');
await wait(15000);
await shot(page, '12-studio-sources', { fullPage: true });
const tile = (name) =>
	page.locator('div.rounded-sm.border.p-2', { has: page.getByText(name, { exact: true }) }).first();
await tile('Presenter camera').getByRole('button', { name: 'Next Up' }).click();
await page.evaluate(() => window.scrollTo(0, 0));
await wait(4000);
await shot(page, '13-studio-next-up');
await page.getByRole('button', { name: /Push to Live/ }).click();
await page.evaluate(() => window.scrollTo(0, 0));
await wait(5000);
await shot(page, '14-studio-on-program');

// ---- Scene with both guests
const guestSources = Object.fromEntries(
	(await api('GET', '/sources')).filter((s) => s.name.endsWith('(guest)')).map((s) => [s.name, s.id]),
);
await page.goto(BASE + '/scenes');
await wait(1200);
await page.fill('#scene-name', 'Interview');
await page.click('button[type=submit]');
await wait(1200);
const scene = (await api('GET', '/scenes')).find((s) => s.name === 'Interview');
await api('POST', `/scenes/${scene.id}/layers`, {
	source_id: guestSources['Jordan (guest)'], x: 0, y: 270, width: 960, height: 540, z_index: 1, fit: 'cover',
});
await api('POST', `/scenes/${scene.id}/layers`, {
	source_id: guestSources['Sam (guest)'], x: 960, y: 270, width: 960, height: 540, z_index: 2, fit: 'cover',
});
await page.reload();
await wait(1500);
await page.getByRole('button', { name: 'Interview' }).first().click();
await wait(1000);
await shot(page, '15-scene-editor');

await page.goto(BASE + '/');
await wait(3000);
await page.getByRole('button', { name: 'Scenes', exact: true }).click();
await wait(800);
await page.getByRole('button', { name: 'Take to Program' }).first().click();
await wait(15000);
await page.evaluate(() => window.scrollTo(0, 0));
await wait(500);
await shot(page, '16-studio-scene-live');

// ---- Output config
await page.getByRole('button', { name: 'Edit output config' }).click();
await wait(800);
await el(page, page.locator('div.fixed.inset-0.z-50 > div.relative.z-50').first(), '17-output-config', 4);
await page.getByRole('button', { name: 'Cancel' }).click();
await wait(500);

// ---- CPU pill + overload banner (load endpoint mocked so the banner is shown)
await el(page, page.locator('h1', { hasText: 'STUDIO' }).locator('..'), '18-cpu-pill', 8);
await page.unroute('**/api/v1/status/load');
await page.route('**/api/v1/status/load', (route) =>
	route.fulfill({
		json: {
			cpu_percent: 96,
			cpu_cores: 4,
			overloaded: true,
			stages: [{ id: guestSources['Jordan (guest)'], name: 'Jordan (guest)', kind: 'ingest', speed: 0.71, behind: true }],
		},
	}),
);
await wait(7500);
await shot(page, '19-overload-banner', { clip: { x: 0, y: 0, width: 1440, height: 260 } });
await page.unroute('**/api/v1/status/load');
await page.route('**/api/v1/status/load', calmLoad);

// ---- Destinations
await page.goto(BASE + '/destinations');
await wait(1200);
await page.fill('#dest-name', 'My YouTube channel');
await page.selectOption('#dest-platform', 'youtube');
await page.fill('#dest-key', 'abcd-efgh-ijkl-mnop-qrst');
await el(page, page.locator('section.panel').first(), '20-destination-add');

// ---- Channel
await page.goto(BASE + '/channel');
await wait(1500);
await shot(page, '21-channel', { fullPage: true });

await browser.close();
procs.forEach((p) => p.kill('SIGKILL'));
console.log('done');
