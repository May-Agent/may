import { useEffect } from 'react';
import { create } from 'zustand';
import {
  type ComputerStreamHandle,
  connectComputerStream,
  sendComputerInput,
} from '../api/computer';
import type { BrowserInputMessage } from '../api/browser';

/**
 * One WebRTC connection to a session's virtual desktop, shared by the
 * Computer tab's thumbnail and the full-screen viewer. Opens with the
 * first viewer and closes shortly after the last one goes away.
 */
interface ComputerStreamState {
  sessionId: string | null;
  stream: MediaStream | null;
  state: RTCPeerConnectionState;
}

export const useComputerStreamStore = create<ComputerStreamState>(() => ({
  sessionId: null,
  stream: null,
  state: 'new',
}));

const CLOSE_DELAY_MS = 1500;

let handle: ComputerStreamHandle | null = null;
let generation = 0;
let users = 0;
let closeTimer: ReturnType<typeof setTimeout> | null = null;

function close(): void {
  generation += 1;
  handle?.close();
  handle = null;
  useComputerStreamStore.setState({ sessionId: null, stream: null, state: 'new' });
}

async function open(sessionId: string): Promise<void> {
  const gen = ++generation;
  const set = (patch: Partial<ComputerStreamState>) => {
    if (gen === generation) useComputerStreamStore.setState(patch);
  };
  useComputerStreamStore.setState({ sessionId, stream: null, state: 'connecting' });
  try {
    const next = await connectComputerStream({
      sessionId,
      onTrack: (stream) => set({ stream }),
      onConnectionStateChange: (state) => set({ state }),
    });
    if (gen !== generation) {
      next.close();
      return;
    }
    handle = next;
  } catch {
    set({ state: 'failed' });
  }
}

function acquire(sessionId: string): void {
  users += 1;
  if (closeTimer) {
    clearTimeout(closeTimer);
    closeTimer = null;
  }
  const current = useComputerStreamStore.getState();
  const healthy = current.state !== 'failed' && current.state !== 'closed';
  if (current.sessionId === sessionId && healthy) return;
  close();
  void open(sessionId);
}

function release(): void {
  users = Math.max(0, users - 1);
  if (users > 0 || closeTimer) return;
  closeTimer = setTimeout(() => {
    closeTimer = null;
    if (users === 0) close();
  }, CLOSE_DELAY_MS);
}

/** Keep the desktop stream open while `enabled`; returns its state. */
export function useComputerStream(sessionId: string | null, enabled: boolean): ComputerStreamState {
  useEffect(() => {
    if (!enabled || !sessionId) return;
    acquire(sessionId);
    return release;
  }, [sessionId, enabled]);
  return useComputerStreamStore();
}

/** Send mouse or keyboard input over the open stream, if any. */
export function sendStreamInput(msg: BrowserInputMessage): void {
  if (handle) sendComputerInput(handle.dc, msg);
}
