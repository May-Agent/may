import type {
  ApiErrorCode,
  ChatStreamEvent,
  HoldReason,
  ReplyTranslation,
  ToolSummary,
  ToolVariant,
  TaskRow,
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
  | { kind: 'text'; text: string; seq: number; translation?: ReplyTranslation }
  | { kind: 'tool'; toolId: string; seq: number };

export interface TurnState {
  turnId: string;
  sessionId: string;
  /**
   * Names the reply while it streams; once the turn is done, the id of its
   * first stored assistant message, which the session's messages carry too.
   */
  assistantId: string;
  /** Ids of the assistant messages the finished turn stored. */
  messageIds: string[];
  text: string;
  reasoning: string;
  /**
   * Duration the backend measured for the last reasoning phase, in
   * milliseconds. Non-null even when `reasoning` stayed empty — an empty
   * phase renders as a non-expandable "thought for Ns" header.
   */
  reasoningMs: number | null;
  tools: TurnToolState[];
  /**
   * Chronological segments, preserving the order in which text chunks and
   * tool invocations arrived. Adjacent text chunks coalesce into one segment;
   * each tool_start pushes a new tool segment. This keeps "let me search…"
   * narration visually between the tool it introduces and the one after.
   */
  segments: TurnSegment[];
  todo: { rows: TaskRow[] } | null;
  error: TurnErrorState | null;
  status: 'streaming' | 'done';
  /** Latest decode-rate sample reported by the backend, in tokens/second. */
  tokensPerSecond: number | null;
  /** Set while the turn waits out a quota reset or a provider retry. */
  hold: ({ message: string } & HoldReason) | null;
  /**
   * Where the model responses that finished end: what follows is the
   * response in flight, which a `retract` discards. A tool result closes the
   * response that called it.
   */
  responseFloor: { reasoning: number; segments: number };
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
    messageIds: [],
    text: '',
    reasoning: '',
    reasoningMs: null,
    tools: [],
    segments: [],
    todo: null,
    error: null,
    status: 'streaming',
    tokensPerSecond: null,
    hold: null,
    responseFloor: { reasoning: 0, segments: 0 },
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

/**
 * A tool event's summary replaces the stored one, except for the card: the
 * daemon reports it on its own `tool_card` event, which later start, delta
 * and end summaries do not repeat.
 */
function mergeSummary(existing: ToolSummary, incoming: ToolSummary | undefined): ToolSummary {
  if (!incoming) return existing;
  return incoming.card === undefined && existing.card !== undefined
    ? { ...incoming, card: existing.card }
    : incoming;
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
  // A card change carries no `seq`: it is not part of the turn's ordered
  // events and applies whenever it arrives.
  if (event.kind === 'tool_card') {
    return {
      ...state,
      tools: state.tools.map((tool) =>
        tool.id === event.id
          ? { ...tool, summary: { ...tool.summary, card: event.card } }
          : tool,
      ),
    };
  }
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
        hold: null,
        lastSeq: event.seq,
      };

    case 'translated': {
      // The translation belongs to the reply streamed last.
      const index = state.segments.map((s) => s.kind).lastIndexOf('text');
      if (index < 0) return { ...state, lastSeq: event.seq };
      const segments = state.segments.slice();
      const segment = segments[index]!;
      if (segment.kind === 'text') {
        const { text, language, languageName } = event;
        segments[index] = { ...segment, translation: { text, language, languageName } };
      }
      return { ...state, segments, lastSeq: event.seq };
    }

    case 'reasoning':
      return {
        ...state,
        reasoning: state.reasoning + event.chunk,
        hold: null,
        lastSeq: event.seq,
      };

    case 'reasoning_end':
      return { ...state, reasoningMs: event.seconds * 1000, lastSeq: event.seq };

    case 'tool_start': {
      const tools = upsertTool(
        state.tools,
        event.id,
        (existing) => ({
          ...existing,
          name: event.name,
          variant: event.variant,
          argumentsRaw: event.arguments,
          summary: mergeSummary(existing.summary, event.summary),
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
        hold: null,
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
          summary: mergeSummary(existing.summary, event.summary),
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
        hold: null,
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
          summary: mergeSummary(existing.summary, event.summary),
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
      const segments = appendToolSegment(state.segments, event.id, event.seq);
      return {
        ...state,
        tools,
        segments,
        responseFloor: { reasoning: state.reasoning.length, segments: segments.length },
        lastSeq: event.seq,
      };
    }

    case 'todo_update':
      return {
        ...state,
        todo: { rows: event.rows },
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
    case 'paused': {
      // A turn paused before it stored a reply has none to carry.
      const reply = event.assistant;
      const finalText = reply?.content || state.text;
      const hasTextSegment = state.segments.some((s) => s.kind === 'text');
      const segments =
        !hasTextSegment && finalText.length > 0
          ? [...state.segments, { kind: 'text', text: finalText, seq: event.seq } as TurnSegment]
          : state.segments;
      return {
        ...state,
        assistantId: reply?.id ?? state.assistantId,
        messageIds: event.messageIds,
        text: finalText,
        segments,
        status: 'done',
        lastSeq: event.seq,
      };
    }

    case 'end':
      return { ...state, status: 'done', lastSeq: event.seq };

    case 'ping':
      return { ...state, lastSeq: event.seq };

    case 'speed':
      return { ...state, tokensPerSecond: event.tokensPerSecond, lastSeq: event.seq };

    case 'hold': {
      const { kind: _kind, seq: _seq, turnId: _turnId, ...hold } = event;
      return { ...state, hold, lastSeq: event.seq };
    }

    // The turn's message leaves the conversation; the stores handle it.
    case 'withdrawn':
      return { ...state, lastSeq: event.seq };

    case 'retract': {
      // Only the response in flight goes: its text, its reasoning and the
      // tool calls it began to stream. Finished responses stay.
      const segments = state.segments.slice(0, state.responseFloor.segments);
      const kept = new Set(segments.flatMap((s) => (s.kind === 'tool' ? [s.toolId] : [])));
      return {
        ...state,
        text: segments.flatMap((s) => (s.kind === 'text' ? [s.text] : [])).join(''),
        reasoning: state.reasoning.slice(0, state.responseFloor.reasoning),
        reasoningMs: null,
        segments,
        tools: state.tools.filter((t) => kept.has(t.id)),
        lastSeq: event.seq,
      };
    }
  }
}

/** Fold a list of events into a final turn state. Useful for tests. */
export function reduceEvents(events: ChatStreamEvent[]): TurnState {
  let state: TurnState = emptyTurn('', '', '');
  for (const evt of events) state = applyEvent(state, evt);
  return state;
}
