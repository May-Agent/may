import { create } from 'zustand';
import type { PermissionRequest } from '@/lib/api';
import type { PendingUserMessage, SessionRuntimeState } from '@/types';

interface SessionRuntime {
  state: SessionRuntimeState;
  queuedCount: number;
  waitingPermission: PermissionRequest | null;
  pendingImmediate: PendingUserMessage | null;
  queuedMessages: PendingUserMessage[];
  firstTokenAt?: number;
}

interface RuntimeStoreState {
  sessions: Record<string, SessionRuntime>;
  ensureSession: (sessionId: string) => void;
  setStreaming: (sessionId: string) => void;
  setQueuedCount: (sessionId: string, count: number) => void;
  setQueuedMessages: (
    sessionId: string,
    pendingImmediate: PendingUserMessage | null,
    queuedMessages: PendingUserMessage[],
  ) => void;
  setWaitingPermission: (sessionId: string, request: PermissionRequest | null) => void;
  setServerState: (sessionId: string, state: SessionRuntimeState) => void;
  markFirstToken: (sessionId: string) => void;
  clearSessionRuntime: (sessionId: string) => void;
}

const defaultRuntime = (): SessionRuntime => ({
  state: 'idle',
  queuedCount: 0,
  waitingPermission: null,
  pendingImmediate: null,
  queuedMessages: [],
});

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

  setStreaming: (sessionId) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          state: 'running',
          waitingPermission: null,
        },
      },
    }));
  },

  setQueuedCount: (sessionId, count) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          queuedCount: Math.max(0, count),
        },
      },
    }));
  },

  setQueuedMessages: (sessionId, pendingImmediate, queuedMessages) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          pendingImmediate,
          queuedMessages,
          queuedCount: queuedMessages.length,
          state: pendingImmediate
            ? 'pending_immediate'
            : queuedMessages.length > 0
              ? 'queued'
              : (state.sessions[sessionId]?.state ?? 'idle'),
        },
      },
    }));
  },

  setWaitingPermission: (sessionId, request) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          state: request ? 'waiting_permission' : 'idle',
          waitingPermission: request,
        },
      },
    }));
  },

  setServerState: (sessionId, nextState) => {
    if (!sessionId) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultRuntime()),
          state: nextState,
          waitingPermission:
            nextState === 'waiting_permission'
              ? (state.sessions[sessionId]?.waitingPermission ?? null)
              : null,
        },
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
