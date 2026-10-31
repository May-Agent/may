import type { ChatRequestPayload, ChatStreamEvent } from '../types';
import type { SessionBackground } from './background';
import { ApiError, apiSseUrl } from './client';
import { parseNamedSseStream, type NamedSseEvent } from '../stream/sse';

export interface TurnStreamHandle {
  events: AsyncGenerator<ChatStreamEvent, void, void>;
  abort: () => void;
}

interface SessionTurnFrame {
  sessionId: string;
  event: ChatStreamEvent['kind'];
  data: Record<string, unknown>;
}

export interface SessionTurnsOptions {
  /**
   * Also subscribe to the session's `session_background` frames (terminal
   * jobs and subagent runs) on the same stream and hand each snapshot here.
   */
  onBackground?: (snapshot: SessionBackground) => void;
  /**
   * Index of the last turn frame already received: a reopened stream sends
   * only the frames after it instead of replaying the turn in progress.
   */
  after?: string;
  /** Called with each turn frame's index as it arrives. */
  onFrameId?: (id: string) => void;
  /**
   * Called when frames this client missed are gone (their turn ended): the
   * session must be reloaded. The turn in progress follows from its start.
   */
  onResync?: () => void;
}

/**
 * Follow every turn of one session: the `session_turn` frames of the
 * multiplexed `/api/events` stream. Turns run in the daemon whichever client
 * started them; a turn already in progress is replayed from its start.
 */
export async function openSessionTurns(
  sessionId: string,
  abort?: AbortSignal,
  options: SessionTurnsOptions = {},
): Promise<TurnStreamHandle> {
  const controller = new AbortController();
  if (abort) {
    if (abort.aborted) controller.abort();
    else abort.addEventListener('abort', () => controller.abort(), { once: true });
  }
  const topics = options.onBackground ? 'session_turn,session_background' : 'session_turn';
  const params = new URLSearchParams({ topics, sessionId });
  if (options.after) params.set('after', options.after);
  const res = await fetch(apiSseUrl(`/events?${params.toString()}`), {
    headers: { Accept: 'text/event-stream' },
    signal: controller.signal,
  });
  if (!res.ok) {
    throw new ApiError('internal_error', `turn stream failed: ${res.status}`, res.status);
  }
  return {
    events: turnEvents(
      parseNamedSseStream<Record<string, unknown>>(res, controller.signal),
      sessionId,
      options,
    ),
    abort: () => controller.abort(),
  };
}

async function* turnEvents(
  source: AsyncGenerator<NamedSseEvent<Record<string, unknown>>, void, void>,
  sessionId: string,
  options: SessionTurnsOptions,
): AsyncGenerator<ChatStreamEvent, void, void> {
  for await (const { event, data, id } of source) {
    if (id) options.onFrameId?.(id);
    if (event === 'session_resync') {
      options.onResync?.();
      continue;
    }
    if (!data || typeof data !== 'object') continue;
    if (event === 'session_background') {
      const snapshot = data as unknown as SessionBackground;
      if (snapshot.sessionId === sessionId) options.onBackground?.(snapshot);
      continue;
    }
    if (event !== 'session_turn') continue;
    const frame = data as unknown as SessionTurnFrame;
    if (frame.sessionId !== sessionId || !frame.data) continue;
    yield { ...frame.data, kind: frame.event } as unknown as ChatStreamEvent;
  }
}

/** Start a turn; it streams through {@link openSessionTurns}. */
export async function sendMessage(payload: ChatRequestPayload): Promise<void> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(payload.sessionId)}/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!res.ok) {
    const message = await res.text().catch(() => '');
    throw new ApiError(
      res.status === 409 ? 'bad_request' : 'internal_error',
      message || `send failed: ${res.status}`,
      res.status,
    );
  }
}

export async function enqueueMessage(payload: ChatRequestPayload): Promise<void> {
  const res = await fetch('/api/chat/enqueue', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!res.ok) throw new ApiError('internal_error', 'enqueue failed', res.status);
}

export async function stopSession(sessionId: string): Promise<void> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/stop`, {
    method: 'POST',
  });
  if (!res.ok) throw new ApiError('internal_error', 'stop failed', res.status);
}
