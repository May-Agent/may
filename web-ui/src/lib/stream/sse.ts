/**
 * Robust SSE parser — consumes a `ReadableStream<Uint8Array>` and yields
 * parsed event payloads. Follows the EventStream spec: events are separated
 * by a blank line, each event is a set of `field: value` lines.
 *
 * We only care about the `data:` field — everything else (id, event,
 * comments) is ignored to stay honest to our server which emits exclusively
 * `data: <json>\n\n`.
 */
export async function* parseSseStream<T>(
  response: Response,
  abort?: AbortSignal,
): AsyncGenerator<T, void, void> {
  if (!response.body) {
    throw new Error('sse response has no body');
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';

  const onAbort = () => reader.cancel().catch(() => undefined);
  abort?.addEventListener('abort', onAbort, { once: true });

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      for (;;) {
        const idx = buffer.indexOf('\n\n');
        if (idx < 0) break;
        const rawEvent = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        const parsed = parseEventBlock<T>(rawEvent);
        if (parsed !== null) yield parsed;
      }
    }

    buffer += decoder.decode();
    const tail = buffer.trim();
    if (tail.length > 0) {
      const parsed = parseEventBlock<T>(tail);
      if (parsed !== null) yield parsed;
    }
  } finally {
    abort?.removeEventListener('abort', onAbort);
    reader.releaseLock();
  }
}

function parseEventBlock<T>(block: string): T | null {
  const dataLines: string[] = [];
  for (const line of block.split('\n')) {
    if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).replace(/^ /, ''));
    }
  }
  if (dataLines.length === 0) return null;
  const payload = dataLines.join('\n');
  try {
    return JSON.parse(payload) as T;
  } catch {
    return null;
  }
}

export interface NamedSseEvent<T> {
  event: string;
  data: T;
}

/**
 * Parse SSE preserving the `event:` field name. Useful when the server uses
 * distinct event names for different payload types (e.g. "artifacts" vs
 * "heartbeat") instead of a single discriminated JSON kind.
 */
export async function* parseNamedSseStream<T>(
  response: Response,
  abort?: AbortSignal,
): AsyncGenerator<NamedSseEvent<T>, void, void> {
  if (!response.body) throw new Error('sse response has no body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';

  const onAbort = () => reader.cancel().catch(() => undefined);
  abort?.addEventListener('abort', onAbort, { once: true });

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      for (;;) {
        const idx = buffer.indexOf('\n\n');
        if (idx < 0) break;
        const rawEvent = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        const parsed = parseNamedEventBlock<T>(rawEvent);
        if (parsed !== null) yield parsed;
      }
    }
    buffer += decoder.decode();
    const tail = buffer.trim();
    if (tail.length > 0) {
      const parsed = parseNamedEventBlock<T>(tail);
      if (parsed !== null) yield parsed;
    }
  } finally {
    abort?.removeEventListener('abort', onAbort);
    reader.releaseLock();
  }
}

function parseNamedEventBlock<T>(block: string): NamedSseEvent<T> | null {
  let event = 'message';
  const dataLines: string[] = [];
  for (const line of block.split('\n')) {
    if (line.startsWith('event:')) {
      event = line.slice(6).trim();
    } else if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).replace(/^ /, ''));
    }
  }
  if (dataLines.length === 0) return { event, data: undefined as T };
  const payload = dataLines.join('\n');
  try {
    return { event, data: JSON.parse(payload) as T };
  } catch {
    return null;
  }
}
