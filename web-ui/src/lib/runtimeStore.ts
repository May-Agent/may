import { create } from 'zustand';
import type {
  AskUserRequest,
  PermissionRequest,
  SessionRuntimeSnapshot,
  SessionRuntimeStatus,
} from '@/lib/api';
import type { PendingUserMessage, SessionRuntimeState } from '@/types';
import type { SessionPlanState } from '@/types';

interface SessionRuntime {
  state: SessionRuntimeState;
  planState: SessionPlanState;
  queuedCount: number;
  waitingPermission: PermissionRequest | null;
  waitingAskUser: AskUserRequest | null;
  pendingImmediate: PendingUserMessage | null;
  queuedMessages: PendingUserMessage[];
  firstTokenAt?: number;
}

interface RuntimeStoreState {
  sessions: Record<string, SessionRuntime>;
  ensureSession: (sessionId: string) => void;
  markRunning: (sessionId: string) => void;
  applyRuntime: (sessionId: string, runtime: SessionRuntimeStatus) => void;
  applyRuntimes: (sessions: SessionRuntimeSnapshot[]) => void;
  markFirstToken: (sessionId: string) => void;
  clearSessionRuntime: (sessionId: string) => void;
}

const defaultRuntime = (): SessionRuntime => ({
  state: 'idle',
  planState: 'disabled',
  queuedCount: 0,
  waitingPermission: null,
  waitingAskUser: null,
  pendingImmediate: null,
  queuedMessages: [],
});

function runtimeFromStatus(
  runtime: SessionRuntimeStatus,
  current?: SessionRuntime,
): SessionRuntime {
  return {
    ...defaultRuntime(),
    firstTokenAt: current?.firstTokenAt,
    state: runtime.state,
    planState: runtime.planState ?? current?.planState ?? 'disabled',
    queuedCount: Math.max(0, runtime.queuedCount ?? 0),
    waitingPermission: runtime.waitingPermission ?? null,
    waitingAskUser: runtime.waitingAskUser ?? null,
    pendingImmediate: runtime.pendingImmediate ?? null,
    queuedMessages: runtime.queuedMessages ?? [],
  };
}

export const useRuntimeStore = create<RuntimeStoreState>((set, get) => ({
  sessions: {},

  ensureSession: (sessionId) => {
    if (!sessionId || get().sessions[sessionId]) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: defaultRuntime(),
      },
    }));
  },

  markRunning: (sessionId) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          state: 'running',
          waitingPermission: null,
          waitingAskUser: null,
        },
      },
    }));
  },

  applyRuntime: (sessionId, runtime) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: runtimeFromStatus(runtime, state.sessions[sessionId]),
      },
    }));
  },

  applyRuntimes: (sessions) => {
    if (sessions.length === 0) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        ...Object.fromEntries(
          sessions.map(({ sessionId, runtime }) => [
            sessionId,
            runtimeFromStatus(runtime, state.sessions[sessionId]),
          ]),
        ),
      },
    }));
  },

  markFirstToken: (sessionId) => {
    if (!sessionId) return;
    set((state) => {
      const current = state.sessions[sessionId] ?? defaultRuntime();
      if (current.firstTokenAt) return state;
      return {
        sessions: {
          ...state.sessions,
          [sessionId]: {
            ...current,
            firstTokenAt: Date.now(),
          },
        },
      };
    });
  },

  clearSessionRuntime: (sessionId) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: defaultRuntime(),
      },
    }));
  },
}));
