import { create } from 'zustand';
import type {
  BrowserLifecycleEvent,
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
import { useModelsStore } from './models';
import { useConfigStore } from './config';
import { useBrowserViewStore } from './browserView';
import type { NotificationMailboxEntry } from '../api/notifications';

/// Aggregated browser lifecycle snapshot: the runtime store keeps the
/// most recent lifecycle event plus a few derived counters so consumers
/// can render a compact status pill without replaying the full history.
export interface BrowserLifecycleSnapshot {
  last: BrowserLifecycleEvent | null;
  peerCount: number;
  takeoverSessionId: string | null;
  pageUrl: string | null;
  pageTitle: string | null;
  errorMessage: string | null;
}

const EMPTY_LIFECYCLE: BrowserLifecycleSnapshot = {
  last: null,
  peerCount: 0,
  takeoverSessionId: null,
  pageUrl: null,
  pageTitle: null,
  errorMessage: null,
};

interface MailboxEventPayload {
  items?: NotificationMailboxEntry[];
}

interface RuntimeState {
  connected: boolean;
  activeProfileId: string | null;
  browser: BrowserStatusPayload | null;
  browserLifecycle: BrowserLifecycleSnapshot;
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
  'browser_lifecycle',
  'profile_sessions',
  'session_runtime',
  'session_runtimes',
  'remote_access',
  'notification_mailbox',
  'network_access_logs',
  'request_logs',
];

function updateProfileSessionRuntimes(
  profileSessions: ProfileSessions | null,
  runtimes: Record<string, SessionRuntimeSummary>,
): ProfileSessions | null {
  if (!profileSessions) return null;
  const update = <T extends { id: string; runtime: SessionRuntimeSummary }>(item: T): T => {
    const runtime = runtimes[item.id];
    return runtime ? { ...item, runtime } : item;
  };
  return {
    ...profileSessions,
    pinned: profileSessions.pinned.map((item) =>
      item.type === 'session'
        ? update(item)
        : { ...item, sessions: item.sessions.map(update) },
    ),
    regular: profileSessions.regular.map(update),
    archived: profileSessions.archived.map(update),
  };
}

function findProfileSession(profileSessions: ProfileSessions, sessionId: string) {
  for (const item of profileSessions.pinned) {
    if (item.type === 'session') {
      if (item.id === sessionId) return item;
      continue;
    }
    const nested = item.sessions.find((session) => session.id === sessionId);
    if (nested) return nested;
  }
  return [...profileSessions.regular, ...profileSessions.archived].find(
    (session) => session.id === sessionId,
  );
}

function refreshChangedActiveSession(profileSessions: ProfileSessions) {
  const sessions = useSessionsStore.getState();
  const active = sessions.active;
  if (!active) return;
  const summary = findProfileSession(profileSessions, active.id);
  if (!summary?.updated_at || summary.updated_at === active.lastModified) return;
  void sessions.refreshActive(active.id);
}

function reduceLifecycle(
  prev: BrowserLifecycleSnapshot,
  event: BrowserLifecycleEvent,
): BrowserLifecycleSnapshot {
  switch (event.kind) {
    case 'peerConnected':
      return { ...prev, last: event, peerCount: event.count };
    case 'peerDisconnected':
      return { ...prev, last: event, peerCount: event.count };
    case 'takeoverChanged':
      return { ...prev, last: event, takeoverSessionId: event.sessionId };
    case 'navigating':
      return { ...prev, last: event, pageUrl: event.url, pageTitle: null };
    case 'pageLoaded':
      return {
        ...prev,
        last: event,
        pageUrl: event.url,
        pageTitle: event.title ?? prev.pageTitle,
      };
    case 'error':
      return { ...prev, last: event, errorMessage: event.message };
    case 'stopped':
      return { ...EMPTY_LIFECYCLE, last: event };
    case 'starting':
    case 'ready':
    case 'chromeFetching':
    case 'encoderStalled':
      return { ...prev, last: event, errorMessage: null };
  }
}

export const useRuntimeStore = create<RuntimeState>((set, get) => ({
  connected: false,
  activeProfileId: null,
  browser: null,
  browserLifecycle: EMPTY_LIFECYCLE,
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
            case 'config':
              // Provider/model configuration changed server-side; refresh the
              // shared config snapshot and invalidate the cached models so both
              // reflect the change without a manual refresh.
              useConfigStore.getState().setConfig(evt.payload);
              void useModelsStore.getState().refresh();
              break;
            case 'browser_status':
              set({ browser: evt.payload });
              break;
            case 'browser_lifecycle': {
              const event = evt.payload;
              set((prev) => ({
                browserLifecycle: reduceLifecycle(prev.browserLifecycle, event),
              }));
              // The first time the agent drives the browser, show its screen
              // in the inspector's Browser tab (once per page load).
              if (event.kind === 'navigating' || event.kind === 'ready') {
                useBrowserViewStore.getState().revealBrowserOnce();
              }
              break;
            }
            case 'profile_sessions': {
              const profileSessions = evt.payload.profileSessions;
              set({ profileSessions });
              refreshChangedActiveSession(profileSessions);
              break;
            }
            case 'session_runtime': {
              const { sessionId, runtime } = evt.payload;
              set((prev) => {
                const runtimes = { ...prev.runtimes, [sessionId]: runtime };
                return {
                  runtimes,
                  profileSessions: updateProfileSessionRuntimes(prev.profileSessions, runtimes),
                };
              });
              useSessionsStore.getState().updateRuntime(sessionId, runtime);
              break;
            }
            case 'session_runtimes': {
              const next: Record<string, SessionRuntimeSummary> = {};
              for (const r of evt.payload.sessions) next[r.sessionId] = r.runtime;
              set((prev) => ({
                runtimes: next,
                profileSessions: updateProfileSessionRuntimes(prev.profileSessions, next),
              }));
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
