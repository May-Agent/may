import type { BrowserInputMessage } from './browser';
import { apiGet, apiPost } from './client';

export interface ComputerStatus {
  running: boolean;
}

const base = (sessionId: string) => `/sessions/${sessionId}/computer`;

export function getComputerStatus(sessionId: string): Promise<ComputerStatus> {
  return apiGet<ComputerStatus>(`${base(sessionId)}/status`);
}

export function startComputer(sessionId: string): Promise<ComputerStatus> {
  return apiPost<ComputerStatus>(`${base(sessionId)}/start`, {});
}

export function stopComputer(sessionId: string): Promise<{ ok: boolean }> {
  return apiPost(`${base(sessionId)}/stop`, {});
}

export function startComputerTakeover(sessionId: string): Promise<{ ok: boolean; takeover: boolean }> {
  return apiPost(`${base(sessionId)}/takeover/start`, {});
}

export function stopComputerTakeover(sessionId: string): Promise<{ ok: boolean; takeover: boolean }> {
  return apiPost(`${base(sessionId)}/takeover/stop`, {});
}

interface OfferAnswer {
  sdp: string;
}

export interface ComputerStreamHandle {
  pc: RTCPeerConnection;
  dc: RTCDataChannel;
  close: () => void;
}

export interface ConnectComputerStreamOptions {
  sessionId: string;
  onTrack: (stream: MediaStream) => void;
  onConnectionStateChange?: (state: RTCPeerConnectionState) => void;
}

/** WHEP-style negotiation against the session's /computer/offer. */
export async function connectComputerStream(
  options: ConnectComputerStreamOptions,
): Promise<ComputerStreamHandle> {
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

  const answer = await apiPost<OfferAnswer>(`${base(options.sessionId)}/offer`, {
    sdp: localSdp,
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

export function sendComputerInput(dc: RTCDataChannel, msg: BrowserInputMessage): void {
  if (dc.readyState !== 'open') return;
  dc.send(JSON.stringify(msg));
}
