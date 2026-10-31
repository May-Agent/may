import { useEffect } from 'react';
import { create } from 'zustand';
import {
  type BrowserInputMessage,
  type BrowserStreamHandle,
  connectBrowserStream,
  sendBrowserInput,
} from '../api/browser';

/**
 * One WebRTC connection to the managed browser, shared by every view that
 * shows its screen (the Browser tab's thumbnail and the full-screen
 * viewer). Views ask for it with `useBrowserStream`; it opens with the first
 * and closes shortly after the last one goes away.
 */
interface BrowserStreamState {
  sessionId: string | null;
  stream: MediaStream | null;
  state: RTCPeerConnectionState;
}

export const useBrowserStreamStore = create<BrowserStreamState>(() => ({
  sessionId: null,
  stream: null,
  state: 'new',
}));

const CLOSE_DELAY_MS = 1500;

let handle: BrowserStreamHandle | null = null;
let generation = 0;
let users = 0;
let closeTimer: ReturnType<typeof setTimeout> | null = null;

function close(): void {
  generation += 1;
  handle?.close();
  handle = null;
  useBrowserStreamStore.setState({ sessionId: null, stream: null, state: 'new' });
}

async function open(sessionId: string): Promise<void> {
  const gen = ++generation;
  const set = (patch: Partial<BrowserStreamState>) => {
    if (gen === generation) useBrowserStreamStore.setState(patch);
  };
  useBrowserStreamStore.setState({ sessionId, stream: null, state: 'connecting' });
  try {
    const next = await connectBrowserStream({
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
  const current = useBrowserStreamStore.getState();
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

/** Keep the browser stream open while `enabled`; returns its state. */
export function useBrowserStream(sessionId: string | null, enabled: boolean): BrowserStreamState {
  useEffect(() => {
    if (!enabled || !sessionId) return;
    acquire(sessionId);
    return release;
  }, [sessionId, enabled]);
  return useBrowserStreamStore();
}

/** Send mouse or keyboard input over the open stream, if any. */
export function sendStreamInput(msg: BrowserInputMessage): void {
  if (handle) sendBrowserInput(handle.dc, msg);
}
