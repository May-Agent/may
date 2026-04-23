import { create } from 'zustand';
import type { ApiMessage, ChatRequestPayload, ChatStreamEvent } from '../types';
import { openChatStream } from '../api/chat';
import { applyEvent, emptyTurn, type TurnState } from '../stream/reducer';
import { useSessionsStore } from './sessions';

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

    set((prev) => ({
      active: null,
      last: prev.active,
      abort: null,
    }));

    if (doneAssistant) {
      useSessionsStore.getState().appendMessage(doneAssistant);
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
