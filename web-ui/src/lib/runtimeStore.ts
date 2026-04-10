import { create } from 'zustand';
import type {
  AskUserRequest,
  PermissionRequest,
  SessionRuntimeSnapshot,
  SessionRuntimeStatus,
  WorkspaceRequest,
} from '@/lib/api';
import type { PendingUserMessage, SessionRuntimeState } from '@/types';
import type { SessionPlanState } from '@/types';

interface SessionRuntime {
  state: SessionRuntimeState;
  planState: SessionPlanState;
  interactions: {
    browser: {
      takeoverActive: boolean;
    };
    terminal: {
      taskId: string | null;
      outputFile: string | null;
      outputPreview: string | null;
      inputNotice: string | null;
    };
  };
  queuedCount: number;
  waitingPermission: PermissionRequest | null;
  waitingWorkspace: WorkspaceRequest | null;
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
  removeSession: (sessionId: string) => void;
}

const defaultRuntime = (): SessionRuntime => ({
  state: 'idle',
  planState: 'disabled',
  interactions: {
    browser: { takeoverActive: false },
    terminal: { taskId: null, outputFile: null, outputPreview: null, inputNotice: null },
  },
  queuedCount: 0,
  waitingPermission: null,
  waitingWorkspace: null,
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
    planState: runtime.planState,
    interactions: runtime.interactions ?? defaultRuntime().interactions,
    queuedCount: Math.max(0, runtime.queuedCount ?? 0),
    waitingPermission: runtime.waitingPermission ?? null,
    waitingWorkspace: runtime.waitingWorkspace ?? null,
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
          waitingWorkspace: null,
          waitingAskUser: null,
          // Clear stale queued state from previous run
          queuedCount: 0,
          pendingImmediate: null,
          queuedMessages: [],
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

  removeSession: (sessionId) => {
    if (!sessionId) return;
    set((state) => {
      const { [sessionId]: _, ...rest } = state.sessions;
      return { sessions: rest };
    });
  },
}));
