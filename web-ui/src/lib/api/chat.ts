import type { ChatRequestPayload, ChatStreamEvent } from '../types';
import { ApiError, apiSseUrl } from './client';
import { parseNamedSseStream } from '../stream/sse';

export interface ChatStreamHandle {
  events: AsyncGenerator<ChatStreamEvent, void, void>;
  abort: () => void;
}

/**
 * Open a POST SSE stream to `/api/chat/stream`. The returned generator yields
 * typed {@link ChatStreamEvent}s until the server closes the connection.
 */
export async function openChatStream(
  payload: ChatRequestPayload,
  abort?: AbortSignal,
): Promise<ChatStreamHandle> {
  const controller = new AbortController();
  if (abort) {
    if (abort.aborted) controller.abort();
    else abort.addEventListener('abort', () => controller.abort(), { once: true });
  }

  const res = await fetch(apiSseUrl('/chat/stream'), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'text/event-stream',
    },
    body: JSON.stringify(payload),
    signal: controller.signal,
  });

  if (!res.ok) {
    throw new ApiError('internal_error', `chat stream failed: ${res.status}`, res.status);
  }

  return {
    events: mergeEventName(parseNamedSseStream<Record<string, unknown>>(res, controller.signal)),
    abort: () => controller.abort(),
  };
}

async function* mergeEventName(
  source: AsyncGenerator<{ event: string; data: Record<string, unknown> }, void, void>,
): AsyncGenerator<ChatStreamEvent, void, void> {
  for await (const { event, data } of source) {
    if (!data || typeof data !== 'object') continue;
    yield { ...data, kind: event } as unknown as ChatStreamEvent;
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
