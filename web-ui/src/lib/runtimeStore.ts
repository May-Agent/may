import { create } from 'zustand';
import type { PermissionRequest } from '@/lib/api';

export type SessionRuntimeState = 'idle' | 'queued' | 'streaming' | 'waiting_permission';

interface RuntimeSessionState {
  state: SessionRuntimeState;
  queuedCount: number;
  pendingPermission: PermissionRequest | null;
  streamStartAt: number | null;
  firstTokenAt: number | null;
}

interface RuntimeStore {
  sessions: Record<string, RuntimeSessionState>;
  ensureSession: (sessionId: string) => void;
  setStreaming: (sessionId: string) => void;
  setQueuedCount: (sessionId: string, count: number) => void;
  setWaitingPermission: (sessionId: string, permission: PermissionRequest | null) => void;
  setServerState: (sessionId: string, state: SessionRuntimeState) => void;
  markFirstToken: (sessionId: string) => void;
  clearSessionRuntime: (sessionId: string) => void;
}

function defaultSessionState(): RuntimeSessionState {
  return {
    state: 'idle',
    queuedCount: 0,
    pendingPermission: null,
    streamStartAt: null,
    firstTokenAt: null,
  };
}

export const useRuntimeStore = create<RuntimeStore>((set, get) => ({
  sessions: {},

  ensureSession: (sessionId) => {
    if (get().sessions[sessionId]) return;
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: defaultSessionState(),
      },
    }));
  },

  setStreaming: (sessionId) => {
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultSessionState()),
          state: 'streaming',
          pendingPermission: null,
          streamStartAt: Date.now(),
          firstTokenAt: null,
        },
      },
    }));
  },

  setQueuedCount: (sessionId, count) => {
    set((state) => {
      const current = state.sessions[sessionId] ?? defaultSessionState();
      const nextState: SessionRuntimeState =
        count > 0 && current.state === 'idle' ? 'queued' : current.state;

      return {
        sessions: {
          ...state.sessions,
          [sessionId]: {
            ...current,
            queuedCount: count,
            state: nextState,
          },
        },
      };
    });
  },

  setWaitingPermission: (sessionId, permission) => {
    set((state) => {
      const current = state.sessions[sessionId] ?? defaultSessionState();
      const nextState = permission
        ? 'waiting_permission'
        : current.state === 'waiting_permission'
          ? 'idle'
          : current.state;

      return {
        sessions: {
          ...state.sessions,
          [sessionId]: {
            ...current,
            state: nextState,
            pendingPermission: permission,
          },
        },
      };
    });
  },

  setServerState: (sessionId, nextState) => {
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultSessionState()),
          state: nextState,
        },
      },
    }));
  },

  markFirstToken: (sessionId) => {
    set((state) => {
      const current = state.sessions[sessionId] ?? defaultSessionState();
      if (current.firstTokenAt) {
        return state;
      }
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
    set((state) => ({
      sessions: {
        ...state.sessions,
        [sessionId]: {
          ...(state.sessions[sessionId] ?? defaultSessionState()),
          state: 'idle',
          queuedCount: 0,
          pendingPermission: null,
          streamStartAt: null,
          firstTokenAt: null,
        },
      },
    }));
  },
}));
