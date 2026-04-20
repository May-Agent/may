import type {
  ApiErrorCode,
  ChatStreamEvent,
  ToolVariant,
  SseTodoItem,
} from '../types';

export interface TurnToolState {
  id: string;
  name: string;
  variant: ToolVariant;
  /** Raw accumulated JSON argument fragment (may be partial mid-stream). */
  argumentsRaw: string;
  /** Streaming status. */
  status: 'streaming' | 'done';
  /** Parsed result once tool_end arrives. */
  result?: unknown;
  /** Last observed seq — used for ordering. */
  seq: number;
}

export interface TurnErrorState {
  code: ApiErrorCode;
  message: string;
  retryable: boolean;
}

export interface TurnState {
  turnId: string;
  sessionId: string;
  assistantId: string;
  text: string;
  reasoning: string;
  tools: TurnToolState[];
  todo: { items: SseTodoItem[]; total: number; completed: number } | null;
  error: TurnErrorState | null;
  status: 'streaming' | 'done';
  lastSeq: number;
}

export function emptyTurn(
  turnId: string,
  sessionId: string,
  assistantId: string,
): TurnState {
  return {
    turnId,
    sessionId,
    assistantId,
    text: '',
    reasoning: '',
    tools: [],
    todo: null,
    error: null,
    status: 'streaming',
    lastSeq: 0,
  };
}

function upsertTool(tools: TurnToolState[], id: string, apply: (tool: TurnToolState) => TurnToolState, fallback: () => TurnToolState): TurnToolState[] {
  const idx = tools.findIndex((t) => t.id === id);
  if (idx < 0) {
    return [...tools, fallback()];
  }
  const next = tools.slice();
  const existing = next[idx]!;
  next[idx] = apply(existing);
  return next;
}

/**
 * Apply one SSE event to a turn. Returns a new `TurnState` — never mutates.
 * Events with a stale `seq` (<= lastSeq) are silently dropped, keeping the
 * stream reducer deterministic under out-of-order or replayed events.
 */
export function applyEvent(state: TurnState, event: ChatStreamEvent): TurnState {
  if (event.kind !== 'start' && event.seq <= state.lastSeq) {
    return state;
  }

  switch (event.kind) {
    case 'start':
      return {
        ...emptyTurn(event.turnId, event.sessionId, event.assistantId),
        lastSeq: event.seq,
      };

    case 'text':
      return { ...state, text: state.text + event.chunk, lastSeq: event.seq };

    case 'reasoning':
      return { ...state, reasoning: state.reasoning + event.chunk, lastSeq: event.seq };

    case 'tool_start': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments,
          status: 'streaming',
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments,
          status: 'streaming',
          seq: event.seq,
        }),
      );
      return { ...state, tools, lastSeq: event.seq };
    }

    case 'tool_delta': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          argumentsRaw: existing.argumentsRaw + event.argumentsFragment,
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.argumentsFragment,
          status: 'streaming',
          seq: event.seq,
        }),
      );
      return { ...state, tools, lastSeq: event.seq };
    }

    case 'tool_end': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          argumentsRaw: event.arguments ?? existing.argumentsRaw,
          status: 'done',
          result: event.result,
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments ?? '',
          status: 'done',
          result: event.result,
          seq: event.seq,
        }),
      );
      return { ...state, tools, lastSeq: event.seq };
    }

    case 'todo_update':
      return {
        ...state,
        todo: { items: event.items, total: event.total, completed: event.completed },
        lastSeq: event.seq,
      };

    case 'error':
      return {
        ...state,
        error: { code: event.code, message: event.message, retryable: event.retryable },
        status: 'done',
        lastSeq: event.seq,
      };

    case 'done':
      return {
        ...state,
        text: event.assistant.content || state.text,
        status: 'done',
        lastSeq: event.seq,
      };

    case 'ping':
      return { ...state, lastSeq: event.seq };
  }
}

/** Fold a list of events into a final turn state. Useful for tests. */
export function reduceEvents(events: ChatStreamEvent[]): TurnState {
  let state: TurnState = emptyTurn('', '', '');
  for (const evt of events) state = applyEvent(state, evt);
  return state;
}
