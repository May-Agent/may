import { create } from 'zustand';
import type { PermissionRequest } from '@/lib/api';

type RuntimeState = 'idle' | 'streaming' | 'waiting_permission';

interface SessionRuntime {
  state: RuntimeState;
  queuedCount: number;
  waitingPermission: PermissionRequest | null;
  firstTokenAt?: number;
}

interface RuntimeStoreState {
  sessions: Record<string, SessionRuntime>;
  ensureSession: (sessionId: string) => void;
  setStreaming: (sessionId: string) => void;
  setQueuedCount: (sessionId: string, count: number) => void;
  setWaitingPermission: (sessionId: string, request: PermissionRequest | null) => void;
  setServerState: (sessionId: string, state: RuntimeState) => void;
  markFirstToken: (sessionId: string) => void;
  clearSessionRuntime: (sessionId: string) => void;
}

const defaultRuntime = (): SessionRuntime => ({
  state: 'idle',
  queuedCount: 0,
  waitingPermission: null,
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
          state: 'streaming',
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
