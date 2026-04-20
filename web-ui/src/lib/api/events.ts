import type { UiEvent } from '../types';
import { apiSseUrl } from './client';
import { parseSseStream } from '../stream/sse';

export interface UiEventStreamHandle {
  events: AsyncGenerator<UiEvent, void, void>;
  abort: () => void;
}

/**
 * Connect to the multiplexed `/api/events` SSE stream and yield typed UI
 * events. The caller controls subscription via the `topics` CSV (see
 * server-side `UiEventTopics`).
 */
export async function openUiEventStream(
  topics: string[],
  abort?: AbortSignal,
): Promise<UiEventStreamHandle> {
  const controller = new AbortController();
  if (abort) {
    if (abort.aborted) controller.abort();
    else abort.addEventListener('abort', () => controller.abort(), { once: true });
  }

  const qs = topics.length > 0 ? `?topics=${encodeURIComponent(topics.join(','))}` : '';
  const res = await fetch(apiSseUrl(`/events${qs}`), {
    headers: { Accept: 'text/event-stream' },
    signal: controller.signal,
  });
  if (!res.ok) {
    throw new Error(`ui events stream failed: ${res.status}`);
  }

  return {
    events: parseSseStream<UiEvent>(res, controller.signal),
    abort: () => controller.abort(),
  };
}
