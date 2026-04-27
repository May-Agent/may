import { create } from 'zustand';
import type {
  BrowserStatusPayload,
  NetworkAccessLogsResponse,
  ProfileSessions,
  RemoteAccessStatusResponse,
  RequestLogsResponse,
  SessionRuntimeSummary,
  UiEvent,
} from '../types';
import { openUiEventStream } from '../api/events';
import { useSessionsStore } from './sessions';
import type { NotificationMailboxEntry } from '../api/notifications';

interface MailboxEventPayload {
  items?: NotificationMailboxEntry[];
}

interface RuntimeState {
  connected: boolean;
  activeProfileId: string | null;
  browser: BrowserStatusPayload | null;
  runtimes: Record<string, SessionRuntimeSummary>;
  profileSessions: ProfileSessions | null;
  mailbox: NotificationMailboxEntry[];
  networkAccess: NetworkAccessLogsResponse | null;
  requestLogs: RequestLogsResponse | null;
  remoteAccess: RemoteAccessStatusResponse | null;
  abort: (() => void) | null;

  connect: (profileId?: string | null) => Promise<void>;
  disconnect: () => void;
  reconnectForProfile: (profileId: string | null) => Promise<void>;
  seedProfileSessions: (profileId: string | null, payload: ProfileSessions) => void;
}

const SUBSCRIBED_TOPICS = [
  'config',
  'browser_status',
  'profile_sessions',
  'session_runtime',
  'session_runtimes',
  'remote_access',
  'notification_mailbox',
  'network_access_logs',
  'request_logs',
];

export const useRuntimeStore = create<RuntimeState>((set, get) => ({
  connected: false,
  activeProfileId: null,
  browser: null,
  runtimes: {},
  profileSessions: null,
  mailbox: [],
  networkAccess: null,
  requestLogs: null,
  remoteAccess: null,
  abort: null,

  async connect(profileId = null) {
    if (get().abort) return;
    const handle = await openUiEventStream(SUBSCRIBED_TOPICS, { profileId });
    set({ abort: handle.abort, connected: true, activeProfileId: profileId ?? null });

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
              const activeId = useSessionsStore.getState().active?.id;
              if (activeId) {
                const match = evt.payload.sessions.find((r) => r.sessionId === activeId);
                if (match) {
                  useSessionsStore.getState().updateRuntime(activeId, match.runtime);
                }
              }
              break;
            }
            case 'notification_mailbox': {
              const payload = evt.payload as MailboxEventPayload;
              set({ mailbox: payload.items ?? [] });
              break;
            }
            case 'network_access_logs':
              set({ networkAccess: evt.payload });
              break;
            case 'request_logs':
              set({ requestLogs: evt.payload });
              break;
            case 'remote_access':
              set({ remoteAccess: evt.payload.status });
              break;
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

  async reconnectForProfile(profileId) {
    const { activeProfileId, abort } = get();
    if (activeProfileId === profileId && abort) return;
    abort?.();
    set({
      abort: null,
      connected: false,
      activeProfileId: profileId,
      profileSessions: null,
    });
    await get().connect(profileId);
  },

  seedProfileSessions(profileId, payload) {
    if (payload.profile.id !== profileId) return;
    if (get().profileSessions) return;
    set({ profileSessions: payload });
  },
}));
