import { create } from 'zustand';
import type { ApiMessage, ChatRequestPayload, ChatStreamEvent } from '../types';
import { openChatStream } from '../api/chat';
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

interface TurnSliceState {
  active: TurnState | null;
  /** Last turn kept for display even after stream ends. */
  last: TurnState | null;
  /** Aborts the current stream. */
  abort: (() => void) | null;
  /** Payload of the most recent send — used to retry after an error. */
  lastPayload: ChatRequestPayload | null;

  send: (payload: ChatRequestPayload) => Promise<void>;
  cancel: () => void;
  retry: () => Promise<void>;
  dismissError: () => void;
}

export const useTurnStore = create<TurnSliceState>((set, get) => ({
  active: null,
  last: null,
  abort: null,
  lastPayload: null,

  async send(payload) {
    get().cancel();

    // Seed a placeholder turn so the UI paints immediately.
    set({
      active: emptyTurn('', payload.sessionId, ''),
      last: null,
      abort: null,
      lastPayload: payload,
    });

    let handle;
    try {
      handle = await openChatStream(payload);
    } catch (err) {
      set({
        active: null,
        last: {
          ...emptyTurn('', payload.sessionId, ''),
          error: {
            code: 'internal_error',
            message: err instanceof Error ? err.message : String(err),
            retryable: true,
          },
          status: 'done',
        },
      });
      return;
    }

    set({ abort: handle.abort });

    let doneAssistant: ApiMessage | null = null;

    try {
      for await (const evt of handle.events as AsyncGenerator<ChatStreamEvent>) {
        set((prev) => {
          if (!prev.active) return prev;
          return { active: applyEvent(prev.active, evt) };
        });

        if (evt.kind === 'done') {
          doneAssistant = evt.assistant;
        }
      }
    } catch (err) {
      set((prev) => {
        const base = prev.active ?? emptyTurn('', payload.sessionId, '');
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
        };
      });
      return;
    }

    const finalActive = get().active;
    set((prev) => ({
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
      useSessionsStore.getState().appendMessage(merged);
    }
  },

  cancel() {
    const { abort } = get();
    if (abort) abort();
    set({ abort: null });
  },

  async retry() {
    const { lastPayload } = get();
    if (!lastPayload) return;
    await get().send(lastPayload);
  },

  dismissError() {
    set({ last: null });
  },
}));
