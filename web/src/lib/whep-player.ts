// Licensed under the GNU Affero General Public License v3.0 — see LICENSE.

import { api, getSessionToken, getApiKey } from '$lib/api';
import type { IceServer } from '$lib/types';

export type WhepHandle = {
	pc: RTCPeerConnection;
	stream: MediaStream;
	sessionPath: string;
	close: () => Promise<void>;
};

function authHeaders(): HeadersInit {
	const headers: Record<string, string> = {
		'Content-Type': 'application/sdp'
	};
	const token = getSessionToken();
	const key = getApiKey();
	if (token) headers['Authorization'] = `Bearer ${token}`;
	else if (key) headers['X-API-Key'] = key;
	return headers;
}

function waitForIceGathering(pc: RTCPeerConnection, timeoutMs = 3000): Promise<void> {
	if (pc.iceGatheringState === 'complete') return Promise.resolve();
	return new Promise((resolve) => {
		const finish = () => {
			pc.removeEventListener('icegatheringstatechange', check);
			clearTimeout(timer);
			resolve();
		};
		const check = () => {
			if (pc.iceGatheringState === 'complete') finish();
		};
		const timer = setTimeout(finish, timeoutMs);
		pc.addEventListener('icegatheringstatechange', check);
	});
}

/**
 * Connect a recvonly WebRTC session to Program WHEP.
 * Throws on failure — caller should fall back to WS-FLV.
 */
export async function connectProgramWhep(
	iceServers?: IceServer[]
): Promise<WhepHandle> {
	let servers = iceServers;
	if (!servers) {
		try {
			const cfg = await api.getWebrtcConfig();
			servers = cfg.ice_servers;
		} catch {
			servers = [{ urls: ['stun:stun.l.google.com:19302'] }];
		}
	}

	const pc = new RTCPeerConnection({
		iceServers: servers.map((s) => ({
			urls: s.urls,
			username: s.username,
			credential: s.credential
		}))
	});

	const stream = new MediaStream();
	pc.ontrack = (ev) => {
		for (const track of ev.streams[0]?.getTracks() ?? [ev.track]) {
			stream.addTrack(track);
		}
	};

	pc.addTransceiver('video', { direction: 'recvonly' });
	pc.addTransceiver('audio', { direction: 'recvonly' });

	await pc.setLocalDescription(await pc.createOffer());
	await waitForIceGathering(pc);

	const res = await fetch('/api/v1/program/whep', {
		method: 'POST',
		headers: authHeaders(),
		body: pc.localDescription?.sdp ?? ''
	});
	if (!res.ok) {
		pc.close();
		throw new Error(`WHEP failed: ${res.status}`);
	}

	const answer = await res.text();
	const location = res.headers.get('Location') || '';
	await pc.setRemoteDescription({ type: 'answer', sdp: answer });

	return {
		pc,
		stream,
		sessionPath: location,
		close: async () => {
			try {
				if (location) {
					await fetch(location, {
						method: 'DELETE',
						headers: authHeaders()
					});
				}
			} catch {
				/* ignore */
			}
			pc.close();
			stream.getTracks().forEach((t) => t.stop());
		}
	};
}
