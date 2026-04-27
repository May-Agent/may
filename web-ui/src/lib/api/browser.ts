import type { BrowserStatusPayload } from '../types';
import { apiGet, apiPost } from './client';

export function getBrowserStatus(): Promise<BrowserStatusPayload> {
  return apiGet<BrowserStatusPayload>('/browser/status');
}

export function startBrowser(): Promise<BrowserStatusPayload> {
  return apiPost<BrowserStatusPayload>('/browser/start', {});
}

export function stopBrowser(): Promise<BrowserStatusPayload> {
  return apiPost<BrowserStatusPayload>('/browser/stop', {});
}

export function startTakeover(sessionId: string): Promise<{ ok: boolean; sessionId: string }> {
  return apiPost('/browser/takeover/start', { sessionId });
}

export function stopTakeover(sessionId: string): Promise<{ ok: boolean; sessionId: string }> {
  return apiPost('/browser/takeover/stop', { sessionId });
}

export interface BrowserResizeRequest {
  width: number;
  height: number;
  deviceScaleFactor?: number;
}

export function resizeBrowserViewport(req: BrowserResizeRequest): Promise<{ ok: boolean }> {
  return apiPost('/browser/viewport', req);
}

interface OfferAnswer {
  sdp: string;
}

export interface BrowserStreamHandle {
  pc: RTCPeerConnection;
  dc: RTCDataChannel;
  close: () => void;
}

export interface ConnectBrowserStreamOptions {
  sessionId: string;
  onTrack: (stream: MediaStream) => void;
  onConnectionStateChange?: (state: RTCPeerConnectionState) => void;
}

// WHEP-style WebRTC negotiation against /api/browser/offer. Single HTTP
// round-trip: client offer (recvonly video + DataChannel) -> server
// answer with ICE candidates included.
export async function connectBrowserStream(
  options: ConnectBrowserStreamOptions,
): Promise<BrowserStreamHandle> {
  const pc = new RTCPeerConnection({
    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }],
  });

  pc.ontrack = (event) => {
    const stream = event.streams[0];
    if (stream) options.onTrack(stream);
  };

  if (options.onConnectionStateChange) {
    pc.onconnectionstatechange = () => {
      options.onConnectionStateChange?.(pc.connectionState);
    };
  }

  pc.addTransceiver('video', { direction: 'recvonly' });
  const dc = pc.createDataChannel('input', { ordered: true });

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);

  // Wait for ICE gathering so the SDP we send carries candidates inline,
  // matching the server's WHEP-style single round-trip expectation.
  if (pc.iceGatheringState !== 'complete') {
    await new Promise<void>((resolve) => {
      const onChange = () => {
        if (pc.iceGatheringState === 'complete') {
          pc.removeEventListener('icegatheringstatechange', onChange);
          resolve();
        }
      };
      pc.addEventListener('icegatheringstatechange', onChange);
      setTimeout(() => {
        pc.removeEventListener('icegatheringstatechange', onChange);
        resolve();
      }, 5000);
    });
  }

  const localSdp = pc.localDescription?.sdp;
  if (!localSdp) {
    pc.close();
    throw new Error('local SDP missing after ICE gathering');
  }

  const answer = await apiPost<OfferAnswer>('/browser/offer', {
    sdp: localSdp,
    sessionId: options.sessionId,
  });
  await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });

  return {
    pc,
    dc,
    close: () => {
      try {
        dc.close();
      } catch {
        // already closed
      }
      try {
        pc.close();
      } catch {
        // already closed
      }
    },
  };
}

// Wire shape for the input DataChannel; mirrors the actor's
// `InputMessage`. The actor uses serde(tag = "type", rename_all =
// "camelCase") so the variant tags below are exact.
export type BrowserInputMessage =
  | {
      type: 'mouse';
      eventType: 'mousePressed' | 'mouseReleased' | 'mouseMoved' | 'mouseWheel';
      x: number;
      y: number;
      button?: 'left' | 'right' | 'middle';
      clickCount?: number;
      deltaX?: number;
      deltaY?: number;
      modifiers?: number;
    }
  | {
      type: 'keyboard';
      eventType: 'keyDown' | 'keyUp' | 'char' | 'rawKeyDown';
      key?: string;
      code?: string;
      text?: string;
      modifiers?: number;
    }
  | { type: 'navigate'; url: string };

export function sendBrowserInput(dc: RTCDataChannel, msg: BrowserInputMessage): void {
  if (dc.readyState !== 'open') return;
  dc.send(JSON.stringify(msg));
}
