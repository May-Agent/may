import { create } from 'zustand';
import type { ApiMessage, ChatRequestPayload, ChatStreamEvent } from '../types';
import { openChatStream, stopSession } from '../api/chat';
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
  /** Aborts the current stream. */
  abort: (() => void) | null;
  /** Keeps the composer locked until the backend has fully settled a stop request. */
  stopping: boolean;
  /** Payload of the most recent send, used to retry after an error. */
  lastPayload: ChatRequestPayload | null;
}

const IDLE_TURN: SessionTurn = {
  active: null,
  last: null,
  abort: null,
  stopping: false,
  lastPayload: null,
};

interface TurnSliceState {
  /** Turns keyed by the id of the session that owns them. */
  turns: Record<string, SessionTurn>;

  send: (payload: ChatRequestPayload) => Promise<void>;
  cancel: (sessionId: string) => Promise<void>;
  retry: (sessionId: string) => Promise<void>;
  dismissError: (sessionId: string) => void;
}

export const useTurnStore = create<TurnSliceState>((set, get) => {
  const turnOf = (sessionId: string): SessionTurn => get().turns[sessionId] ?? IDLE_TURN;

  /** Applies `fn` to one session's turn; every other session's turn is untouched. */
  const patch = (sessionId: string, fn: (prev: SessionTurn) => Partial<SessionTurn>) =>
    set((state) => {
      const prev = state.turns[sessionId] ?? IDLE_TURN;
      return { turns: { ...state.turns, [sessionId]: { ...prev, ...fn(prev) } } };
    });

  return {
    turns: {},

    async send(payload) {
      const sessionId = payload.sessionId;
      await get().cancel(sessionId);

      // Seed a placeholder turn so the UI paints immediately.
      patch(sessionId, () => ({
        active: emptyTurn('', sessionId, ''),
        last: null,
        abort: null,
        stopping: false,
        lastPayload: payload,
      }));

      let handle;
      try {
        handle = await openChatStream(payload);
      } catch (err) {
        patch(sessionId, () => ({
          active: null,
          last: {
            ...emptyTurn('', sessionId, ''),
            error: {
              code: 'internal_error',
              message: err instanceof Error ? err.message : String(err),
              retryable: true,
            },
            status: 'done',
          },
          stopping: false,
        }));
        return;
      }

      patch(sessionId, () => ({ abort: handle.abort }));

      let doneAssistant: ApiMessage | null = null;

      try {
        for await (const evt of handle.events as AsyncGenerator<ChatStreamEvent>) {
          patch(sessionId, (prev) =>
            prev.active ? { active: applyEvent(prev.active, evt) } : {},
          );

          if (evt.kind === 'done') {
            doneAssistant = evt.assistant;
          }
        }
      } catch (err) {
        if (turnOf(sessionId).stopping) {
          patch(sessionId, (prev) => ({
            active: null,
            last: prev.active ? { ...prev.active, status: 'done' } : prev.last,
            abort: null,
          }));
          return;
        }
        patch(sessionId, (prev) => {
          const base = prev.active ?? emptyTurn('', sessionId, '');
          return {
            active: null,
            last: {
              ...base,
              error: {
                code: 'internal_error',
                message: err instanceof Error ? err.message : String(err),
                retryable: true,
              },
              status: 'done',
            },
            abort: null,
            stopping: false,
          };
        });
        return;
      }

      const finalActive = turnOf(sessionId).active;
      patch(sessionId, (prev) => ({
        active: null,
        last: prev.active,
        abort: null,
      }));

      if (doneAssistant) {
        const tools = finalActive?.tools ?? [];
        const toolEvent = synthesizeToolEvent(tools);
        const reasoning =
          typeof doneAssistant.thinking === 'string' && doneAssistant.thinking.length > 0
            ? doneAssistant.thinking
            : finalActive?.reasoning && finalActive.reasoning.length > 0
              ? finalActive.reasoning
              : doneAssistant.thinking;
        const merged: ApiMessage = {
          ...doneAssistant,
          toolEvent: toolEvent ?? doneAssistant.toolEvent ?? null,
          thinking: reasoning,
        };
        useSessionsStore.getState().appendMessage(sessionId, merged);
      }
    },

    async cancel(sessionId) {
      const { active, stopping } = turnOf(sessionId);
      if (!active || stopping) return;

      patch(sessionId, () => ({ stopping: true }));
      try {
        await stopSession(sessionId);
        turnOf(sessionId).abort?.();
        patch(sessionId, () => ({ abort: null, stopping: false }));
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
