import { create } from 'zustand';
import type {
  BrowserStatusPayload,
  ProfileSessions,
  SessionRuntimeSummary,
  UiEvent,
} from '../types';
import { openUiEventStream } from '../api/events';
import { useSessionsStore } from './sessions';

interface RuntimeState {
  connected: boolean;
  browser: BrowserStatusPayload | null;
  runtimes: Record<string, SessionRuntimeSummary>;
  profileSessions: ProfileSessions | null;
  abort: (() => void) | null;

  connect: () => Promise<void>;
  disconnect: () => void;
}

const SUBSCRIBED_TOPICS = [
  'heartbeat',
  'config',
  'browser_status',
  'profile_sessions',
  'session_runtime',
  'session_runtimes',
  'remote_access',
  'notification_mailbox',
];

export const useRuntimeStore = create<RuntimeState>((set, get) => ({
  connected: false,
  browser: null,
  runtimes: {},
  profileSessions: null,
  abort: null,

  async connect() {
    if (get().abort) return;
    const handle = await openUiEventStream(SUBSCRIBED_TOPICS);
    set({ abort: handle.abort, connected: true });

    (async () => {
      try {
        for await (const evt of handle.events as AsyncGenerator<UiEvent>) {
          switch (evt.kind) {
            case 'heartbeat':
              break;
            case 'browser_status':
              set({ browser: evt.payload });
              break;
            case 'profile_sessions':
              set({ profileSessions: evt.payload.profileSessions });
              break;
            case 'session_runtime': {
              const { sessionId, runtime } = evt.payload;
              set((prev) => ({ runtimes: { ...prev.runtimes, [sessionId]: runtime } }));
              useSessionsStore.getState().updateRuntime(sessionId, runtime);
              break;
            }
            case 'session_runtimes': {
              const next: Record<string, SessionRuntimeSummary> = {};
              for (const r of evt.payload.sessions) next[r.sessionId] = r.runtime;
              set({ runtimes: next });
              break;
            }
            default:
              break;
          }
        }
      } catch {
        // stream aborted or network dropped
      } finally {
        set({ connected: false, abort: null });
      }
    })();
  },

  disconnect() {
    get().abort?.();
    set({ abort: null, connected: false });
  },
}));
