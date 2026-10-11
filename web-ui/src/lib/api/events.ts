import type { UiEvent } from '../types';
import { apiSseUrl } from './client';
import { parseNamedSseStream } from '../stream/sse';

export interface UiEventStreamHandle {
  events: AsyncGenerator<UiEvent, void, void>;
  abort: () => void;
}

export interface UiEventStreamOptions {
  abort?: AbortSignal;
  profileId?: string | null;
}

/**
 * Connect to the multiplexed `/api/events` SSE stream and yield typed UI
 * events. The caller controls subscription via the `topics` CSV (see
 * server-side `UiEventTopics`). When `profileId` is provided, profile-scoped
 * topics (e.g., `profile_sessions`) are filtered to that profile.
 *
 * The backend emits each frame as `data:<json>\nevent:<kind>\n\n` — the event
 * name is the discriminator, the JSON body is the payload. We merge those
 * into the `{ kind, payload }` shape that `UiEvent` models expect.
 */
export async function openUiEventStream(
  topics: string[],
  options: UiEventStreamOptions | AbortSignal = {},
): Promise<UiEventStreamHandle> {
  const opts: UiEventStreamOptions =
    options instanceof AbortSignal ? { abort: options } : options;

  const controller = new AbortController();
  if (opts.abort) {
    if (opts.abort.aborted) controller.abort();
    else opts.abort.addEventListener('abort', () => controller.abort(), { once: true });
  }

  const params = new URLSearchParams();
  if (topics.length > 0) params.set('topics', topics.join(','));
  if (opts.profileId) params.set('profileId', opts.profileId);
  const qs = params.toString();
  const res = await fetch(apiSseUrl(`/events${qs ? `?${qs}` : ''}`), {
    headers: { Accept: 'text/event-stream' },
    signal: controller.signal,
  });
  if (!res.ok) {
    throw new Error(`ui events stream failed: ${res.status}`);
  }

  return {
    events: mergeNamedIntoUiEvents(
      parseNamedSseStream<Record<string, unknown>>(res, controller.signal),
    ),
    abort: () => controller.abort(),
  };
}

async function* mergeNamedIntoUiEvents(
  source: AsyncGenerator<{ event: string; data: Record<string, unknown> }, void, void>,
): AsyncGenerator<UiEvent, void, void> {
  for await (const { event, data } of source) {
    if (event === 'heartbeat') {
      yield { kind: 'heartbeat' } as unknown as UiEvent;
      continue;
    }
    if (!data || typeof data !== 'object') continue;
    yield { kind: event, payload: data } as unknown as UiEvent;
  }
}
