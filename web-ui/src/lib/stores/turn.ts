import { create } from 'zustand';
import type {
  ApiAttachment,
  ApiMessage,
  ChatRequestPayload,
  ChatStreamEvent,
  CommandCard,
} from '../types';
import { sendMessage, stopSession } from '../api/chat';
import {
  applyBackgroundDelta,
  watchSessionEvents,
  type BackgroundState,
  type SessionAgentFrame,
  type SessionTurnFrame,
} from '../api/sessionEvents';
import { getSessionBackground, type SessionBackground } from '../api/background';
import type { ApiPr } from '../api/prs';
import {
  applyEvent,
  emptyTurn,
  type TurnState,
  type TurnToolState,
} from '../stream/reducer';
import { useSessionsStore } from './sessions';
import { useInspectorStore } from './inspector';

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
  /** Whether this client sent the turn in progress. */
  own: boolean;
  /** A withdrawn message of this client's, waiting to return to the composer. */
  restored: RestoredDraft | null;
  /** Warnings the daemon raised for the user while this view was open. */
  warnings: string[];
}

/** The text and attachments of a message the daemon withdrew. */
export interface RestoredDraft {
  content: string;
  attachments: ApiAttachment[];
}

const IDLE_TURN: SessionTurn = {
  active: null,
  last: null,
  stopping: false,
  lastPayload: null,
  own: false,
  restored: null,
  warnings: [],
};

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
  /** The composer took the withdrawn draft back. */
  consumeRestored: (sessionId: string) => void;
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
  const watchers = new Map<string, { count: number; unwatch: () => void }>();

  const finishWithReply = (
    sessionId: string,
    assistant: ApiMessage | null,
    userMessageId: string | null,
  ) => {
    const finalActive = turnOf(sessionId).active;
    patch(sessionId, (prev) => ({ active: null, last: prev.active, stopping: false }));
    if (assistant === null) {
      useSessionsStore.getState().settleTurn(sessionId, userMessageId, null);
      return;
    }
    const toolEvent = synthesizeToolEvent(finalActive?.tools ?? []);
    const reasoning =
      typeof assistant.thinking === 'string' && assistant.thinking.length > 0
        ? assistant.thinking
        : finalActive?.reasoning && finalActive.reasoning.length > 0
          ? finalActive.reasoning
          : assistant.thinking;
    useSessionsStore.getState().settleTurn(sessionId, userMessageId, {
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
        finishWithReply(sessionId, evt.assistant, evt.userMessageId);
        return;
      case 'withdrawn':
        // The message leaves the conversation; the client that sent it gets
        // it back in the composer.
        useSessionsStore.getState().withdrawUserMessage(sessionId, evt.content);
        patch(sessionId, (prev) => ({
          active: null,
          last: null,
          stopping: false,
          restored: prev.own
            ? { content: evt.content, attachments: evt.attachments }
            : prev.restored,
        }));
        return;
      case 'end':
        patch(sessionId, (prev) =>
          prev.active
            ? { active: null, last: applyEvent(prev.active, evt), stopping: false, own: false }
            : { stopping: false, own: false },
        );
        return;
      default:
        patch(sessionId, (prev) => (prev.active ? { active: applyEvent(prev.active, evt) } : {}));
    }
  };

  /**
   * The stream's `reset` says frames this client missed are gone: drop the
   * turn it was following and reload the session's messages.
   */
  const resync = (sessionId: string) => {
    patch(sessionId, () => ({ active: null, stopping: false }));
    void useSessionsStore.getState().refreshActive(sessionId);
  };

  /**
   * Follow `sessionId`'s events on the shared session stream. The session
   * read's `cursor` resumes the stream where the read left off; the REST
   * background snapshot seeds the state `job.*`/`subagent.*` deltas apply
   * to.
   */
  const follow = (sessionId: string) => {
    let closed = false;
    let unwatch: (() => void) | undefined;
    const background: BackgroundState = { jobs: [], subagents: [] };
    // Turns whose prompt is already in the loaded history: a turn that
    // ended between the session read's cursor and its messages arriving
    // replays on the stream; showing it would duplicate the history rows.
    const suppressedTurns = new Set<string>();
    const emitBackground = () => {
      const snapshot: SessionBackground = {
        sessionId,
        jobs: background.jobs,
        subagents: background.subagents,
      };
      useInspectorStore.getState().applyBackground(snapshot);
    };
    const onEvent = (kind: string, data: Record<string, unknown>) => {
      switch (kind) {
        case 'session_agent': {
          const frame = data as unknown as SessionAgentFrame;
          const messageId = frame.body?.messageId;
          if (frame.body?.kind === 'start' && messageId != null) {
            const messages = useSessionsStore.getState().messages[sessionId] ?? [];
            if (messages.some((m) => m.id === `msg-${messageId}`)) {
              suppressedTurns.add(frame.turnId);
            }
          }
          return;
        }
        case 'session_turn': {
          const frame = data as unknown as SessionTurnFrame;
          if (frame.sessionId !== sessionId || !frame.data) return;
          const turnId = frame.data.turnId as string | undefined;
          if (turnId != null && suppressedTurns.has(turnId)) {
            if (frame.event === 'end') suppressedTurns.delete(turnId);
            return;
          }
          if (frame.event === 'tool_card') {
            // The card can change after its turn ended (an answer arrives),
            // so the settled messages take it as well as the turn in progress.
            const { id, card } = frame.data as unknown as { id: string; card: CommandCard | null };
            useSessionsStore.getState().applyToolCard(sessionId, id, card);
          }
          handle(sessionId, { ...frame.data, kind: frame.event } as unknown as ChatStreamEvent);
          return;
        }
        case 'warning': {
          const { message } = data as { message: string };
          patch(sessionId, (prev) => ({ warnings: [...prev.warnings, message] }));
          return;
        }
        case 'reset':
          resync(sessionId);
          return;
        case 'prs.changed': {
          const { prs } = data as unknown as { prs: ApiPr[] };
          useInspectorStore.getState().applyPrs(sessionId, prs);
          return;
        }
        default:
          if (applyBackgroundDelta(background, kind, data)) emitBackground();
      }
    };
    void (async () => {
      // The session read's cursor is the stream's resume point; a later
      // refresh may have moved it, so the latest one wins.
      const cursor =
        useSessionsStore.getState().active?.id === sessionId
          ? useSessionsStore.getState().active?.cursor
          : undefined;
      try {
        const snapshot = await getSessionBackground(sessionId);
        if (closed) return;
        background.jobs = snapshot.jobs;
        background.subagents = snapshot.subagents;
        useInspectorStore.getState().applyBackground(snapshot);
      } catch {
        // The REST read failed; deltas still apply on the empty snapshot.
      }
      if (closed) return;
      void useInspectorStore.getState().loadPrs(sessionId);
      unwatch = watchSessionEvents(sessionId, cursor, (kind, data) => onEvent(kind, data));
    })();
    return () => {
      closed = true;
      unwatch?.();
    };
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
        watchers.set(sessionId, { count: 1, unwatch: follow(sessionId) });
      }
      return () => {
        const watcher = watchers.get(sessionId);
        if (!watcher) return;
        watcher.count -= 1;
        if (watcher.count <= 0) {
          watcher.unwatch();
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
        own: true,
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

    consumeRestored(sessionId) {
      patch(sessionId, () => ({ restored: null }));
    },
  };
});
