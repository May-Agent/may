import type {
  ApiErrorCode,
  ChatStreamEvent,
  ToolSummary,
  ToolVariant,
  SseTodoItem,
} from '../types';

export interface TurnToolState {
  id: string;
  name: string;
  variant: ToolVariant;
  /** Raw accumulated JSON argument fragment (may be partial mid-stream). */
  argumentsRaw: string;
  /**
   * Backend-projected summary fields. Authoritative for display labels;
   * the frontend never re-parses argumentsRaw to derive these.
   */
  summary: ToolSummary;
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

export type TurnSegment =
  | { kind: 'text'; text: string; seq: number }
  | { kind: 'tool'; toolId: string; seq: number };

export interface TurnState {
  turnId: string;
  sessionId: string;
  assistantId: string;
  text: string;
  reasoning: string;
  tools: TurnToolState[];
  /**
   * Chronological segments, preserving the order in which text chunks and
   * tool invocations arrived. Adjacent text chunks coalesce into one segment;
   * each tool_start pushes a new tool segment. This keeps "let me search…"
   * narration visually between the tool it introduces and the one after.
   */
  segments: TurnSegment[];
  todo: { items: SseTodoItem[]; total: number; completed: number } | null;
  error: TurnErrorState | null;
  status: 'streaming' | 'done';
  /** Latest decode-rate sample reported by the backend, in tokens/second. */
  tokensPerSecond: number | null;
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
    segments: [],
    todo: null,
    error: null,
    status: 'streaming',
    tokensPerSecond: null,
    lastSeq: 0,
  };
}

function appendTextSegment(segments: TurnSegment[], chunk: string, seq: number): TurnSegment[] {
  const last = segments[segments.length - 1];
  if (last && last.kind === 'text') {
    const next = segments.slice(0, -1);
    next.push({ kind: 'text', text: last.text + chunk, seq });
    return next;
  }
  return [...segments, { kind: 'text', text: chunk, seq }];
}

function appendToolSegment(segments: TurnSegment[], toolId: string, seq: number): TurnSegment[] {
  if (segments.some((s) => s.kind === 'tool' && s.toolId === toolId)) return segments;
  return [...segments, { kind: 'tool', toolId, seq }];
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
      return {
        ...state,
        text: state.text + event.chunk,
        segments: appendTextSegment(state.segments, event.chunk, event.seq),
        lastSeq: event.seq,
      };

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
          summary: event.summary ?? existing.summary,
          status: 'streaming',
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments,
          summary: event.summary ?? {},
          status: 'streaming',
          seq: event.seq,
        }),
      );
      return {
        ...state,
        tools,
        segments: appendToolSegment(state.segments, event.id, event.seq),
        lastSeq: event.seq,
      };
    }

    case 'tool_delta': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          name: event.name || existing.name,
          variant: event.name ? event.variant : existing.variant,
          argumentsRaw: existing.argumentsRaw + event.argumentsFragment,
          summary: event.summary ?? existing.summary,
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.argumentsFragment,
          summary: event.summary ?? {},
          status: 'streaming',
          seq: event.seq,
        }),
      );
      return {
        ...state,
        tools,
        segments: appendToolSegment(state.segments, event.id, event.seq),
        lastSeq: event.seq,
      };
    }

    case 'tool_end': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          argumentsRaw: event.arguments ?? existing.argumentsRaw,
          summary: event.summary ?? existing.summary,
          status: 'done',
          result: event.result,
          seq: event.seq,
        }),
        () => ({
          id: event.id,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments ?? '',
          summary: event.summary ?? {},
          status: 'done',
          result: event.result,
          seq: event.seq,
        }),
      );
      return {
        ...state,
        tools,
        segments: appendToolSegment(state.segments, event.id, event.seq),
        lastSeq: event.seq,
      };
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

    case 'done': {
      const finalText = event.assistant.content || state.text;
      const hasTextSegment = state.segments.some((s) => s.kind === 'text');
      const segments =
        !hasTextSegment && finalText.length > 0
          ? [...state.segments, { kind: 'text', text: finalText, seq: event.seq } as TurnSegment]
          : state.segments;
      return {
        ...state,
        text: finalText,
        segments,
        status: 'done',
        lastSeq: event.seq,
      };
    }

    case 'ping':
      return { ...state, lastSeq: event.seq };

    case 'speed':
      return { ...state, tokensPerSecond: event.tokensPerSecond, lastSeq: event.seq };
  }
}

/** Fold a list of events into a final turn state. Useful for tests. */
export function reduceEvents(events: ChatStreamEvent[]): TurnState {
  let state: TurnState = emptyTurn('', '', '');
  for (const evt of events) state = applyEvent(state, evt);
  return state;
}
