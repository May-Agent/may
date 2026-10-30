import type { ChatRequestPayload, ChatStreamEvent } from '../types';
import { ApiError, apiSseUrl } from './client';
import { parseNamedSseStream } from '../stream/sse';

export interface TurnStreamHandle {
  events: AsyncGenerator<ChatStreamEvent, void, void>;
  abort: () => void;
}

interface SessionTurnFrame {
  sessionId: string;
  event: ChatStreamEvent['kind'];
  data: Record<string, unknown>;
}

/**
 * Follow every turn of one session: the `session_turn` frames of the
 * multiplexed `/api/events` stream. Turns run in the daemon whichever client
 * started them; a turn already in progress is replayed from its start.
 */
export async function openSessionTurns(
  sessionId: string,
  abort?: AbortSignal,
): Promise<TurnStreamHandle> {
  const controller = new AbortController();
  if (abort) {
    if (abort.aborted) controller.abort();
    else abort.addEventListener('abort', () => controller.abort(), { once: true });
  }
  const params = new URLSearchParams({ topics: 'session_turn', sessionId });
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
    ),
    abort: () => controller.abort(),
  };
}

async function* turnEvents(
  source: AsyncGenerator<{ event: string; data: Record<string, unknown> }, void, void>,
  sessionId: string,
): AsyncGenerator<ChatStreamEvent, void, void> {
  for await (const { event, data } of source) {
    if (event !== 'session_turn' || !data || typeof data !== 'object') continue;
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
