import { create } from 'zustand';
import type { ApiMessage, ChatRequestPayload, ChatStreamEvent } from '../types';
import { openSessionTurns, sendMessage, stopSession } from '../api/chat';
import {
  applyEvent,
  emptyTurn,
  type TurnState,
  type TurnToolState,
} from '../stream/reducer';
import { useSessionsStore } from './sessions';

/**
 * Build the persisted `tool_event` array from the in-memory turn state.
 * The backend's `done` payload omits tool events, so we synthesize the
 * same shape that `PersistedAssistant.extractTools` expects.
 */
function synthesizeToolEvent(tools: TurnToolState[]): unknown[] | null {
  if (tools.length === 0) return null;
  const events: unknown[] = [];
  for (const tool of tools) {
    events.push({
      stage: 'start',
      id: tool.id,
      name: tool.name,
      variant: tool.variant,
      arguments: tool.argumentsRaw,
      seq: tool.seq,
      summary: tool.summary,
    });
    if (tool.status === 'done') {
      events.push({
        stage: 'end',
        id: tool.id,
        name: tool.name,
        variant: tool.variant,
        result: tool.result,
        seq: tool.seq,
        summary: tool.summary,
      });
    }
  }
  return events;
}

/** Everything a single session's turn owns; never shared across sessions. */
export interface SessionTurn {
  active: TurnState | null;
  /** Last turn kept for display even after stream ends. */
  last: TurnState | null;
  /** Keeps the composer locked until the backend has fully settled a stop request. */
  stopping: boolean;
  /** Payload of the most recent send, used to retry after an error. */
  lastPayload: ChatRequestPayload | null;
}

const IDLE_TURN: SessionTurn = {
  active: null,
  last: null,
  stopping: false,
  lastPayload: null,
};

/** Wait before reopening a session's turn stream that dropped. */
const RECONNECT_DELAY_MS = 1000;

interface TurnSliceState {
  /** Turns keyed by the id of the session that owns them. */
  turns: Record<string, SessionTurn>;

  /**
   * Follow a session's turns, whichever client starts them. Returns the
   * function that stops following.
   */
  watch: (sessionId: string) => () => void;
  send: (payload: ChatRequestPayload) => Promise<void>;
  cancel: (sessionId: string) => Promise<void>;
  retry: (sessionId: string) => Promise<void>;
  dismissError: (sessionId: string) => void;
}

function errorTurn(base: TurnState | null, sessionId: string, err: unknown): TurnState {
  return {
    ...(base ?? emptyTurn('', sessionId, '')),
    error: {
      code: 'internal_error',
      message: err instanceof Error ? err.message : String(err),
      retryable: true,
    },
    status: 'done',
  };
}

export const useTurnStore = create<TurnSliceState>((set, get) => {
  const turnOf = (sessionId: string): SessionTurn => get().turns[sessionId] ?? IDLE_TURN;

  /** Applies `fn` to one session's turn; every other session's turn is untouched. */
  const patch = (sessionId: string, fn: (prev: SessionTurn) => Partial<SessionTurn>) =>
    set((state) => {
      const prev = state.turns[sessionId] ?? IDLE_TURN;
      return { turns: { ...state.turns, [sessionId]: { ...prev, ...fn(prev) } } };
    });

  /** Watchers by session id, counted so nested views share one stream. */
  const watchers = new Map<string, { count: number; controller: AbortController }>();

  const finishWithReply = (sessionId: string, assistant: ApiMessage) => {
    const finalActive = turnOf(sessionId).active;
    patch(sessionId, (prev) => ({ active: null, last: prev.active, stopping: false }));
    const toolEvent = synthesizeToolEvent(finalActive?.tools ?? []);
    const reasoning =
      typeof assistant.thinking === 'string' && assistant.thinking.length > 0
        ? assistant.thinking
        : finalActive?.reasoning && finalActive.reasoning.length > 0
          ? finalActive.reasoning
          : assistant.thinking;
    useSessionsStore.getState().appendMessage(sessionId, {
      ...assistant,
      toolEvent: toolEvent ?? assistant.toolEvent ?? null,
      thinking: reasoning,
    });
  };

  const handle = (sessionId: string, evt: ChatStreamEvent) => {
    switch (evt.kind) {
      case 'start':
        patch(sessionId, (prev) => ({
          active: applyEvent(prev.active ?? emptyTurn('', sessionId, ''), evt),
          last: null,
        }));
        return;
      case 'done':
      case 'paused':
        patch(sessionId, (prev) => (prev.active ? { active: applyEvent(prev.active, evt) } : {}));
        finishWithReply(sessionId, evt.assistant);
        return;
      case 'end':
        patch(sessionId, (prev) =>
          prev.active
            ? { active: null, last: applyEvent(prev.active, evt), stopping: false }
            : { stopping: false },
        );
        return;
      default:
        patch(sessionId, (prev) => (prev.active ? { active: applyEvent(prev.active, evt) } : {}));
    }
  };

  const follow = async (sessionId: string, signal: AbortSignal) => {
    while (!signal.aborted) {
      try {
        const stream = await openSessionTurns(sessionId, signal);
        for await (const evt of stream.events) handle(sessionId, evt);
      } catch (err) {
        if (signal.aborted) return;
        console.warn('session turn stream dropped', err);
      }
      if (signal.aborted) return;
      await new Promise((resolve) => setTimeout(resolve, RECONNECT_DELAY_MS));
    }
  };

  const ensureWatched = (sessionId: string) => {
    if (!watchers.has(sessionId)) get().watch(sessionId);
  };

  return {
    turns: {},

    watch(sessionId) {
      const existing = watchers.get(sessionId);
      if (existing) {
        existing.count += 1;
      } else {
        const controller = new AbortController();
        watchers.set(sessionId, { count: 1, controller });
        void follow(sessionId, controller.signal);
      }
      return () => {
        const watcher = watchers.get(sessionId);
        if (!watcher) return;
        watcher.count -= 1;
        if (watcher.count <= 0) {
          watcher.controller.abort();
          watchers.delete(sessionId);
        }
      };
    },

    async send(payload) {
      const sessionId = payload.sessionId;
      ensureWatched(sessionId);
      // Seed a placeholder turn so the UI paints immediately; the `start`
      // event of the turn replaces it.
      patch(sessionId, (prev) => ({
        active: prev.active ?? emptyTurn('', sessionId, ''),
        last: null,
        stopping: false,
        lastPayload: payload,
      }));
      try {
        await sendMessage(payload);
      } catch (err) {
        patch(sessionId, (prev) => ({
          active: null,
          last: errorTurn(prev.active, sessionId, err),
          stopping: false,
        }));
      }
    },

    async cancel(sessionId) {
      const { active, stopping } = turnOf(sessionId);
      if (!active || stopping) return;

      patch(sessionId, () => ({ stopping: true }));
      try {
        await stopSession(sessionId);
      } catch (err) {
        patch(sessionId, (prev) => ({
          active: prev.active
            ? {
                ...prev.active,
                error: {
                  code: 'internal_error',
                  message: err instanceof Error ? err.message : String(err),
                  retryable: true,
                },
              }
            : null,
          stopping: false,
        }));
      }
    },

    async retry(sessionId) {
      const { lastPayload } = turnOf(sessionId);
      if (!lastPayload) return;
      await get().send(lastPayload);
    },

    dismissError(sessionId) {
      patch(sessionId, () => ({ last: null }));
    },
  };
});
