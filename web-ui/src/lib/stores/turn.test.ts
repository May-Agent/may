import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import type { ApiMessage, ApiSession } from '../types';
import { useSessionsStore } from './sessions';
import { useTurnStore } from './turn';

const originalFetch = globalThis.fetch;
const encoder = new TextEncoder();

function session(id: string, messages: ApiMessage[]): ApiSession {
  return {
    id,
    title: id,
    lastModified: '2026-01-01T00:00:00Z',
    preview: '',
    messages,
    profileId: null,
    model: 'm',
    runtime: {},
  } as ApiSession;
}

function message(id: string, role: 'user' | 'assistant', content: string): ApiMessage {
  return { id, role, content, timestamp: '2026-01-01T00:00:00Z' };
}

function sse(event: string, data: Record<string, unknown>): Uint8Array {
  return encoder.encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

interface Backend {
  sessions: Record<string, ApiSession>;
  /** Controllers of open chat streams, by session id. */
  streams: Record<string, ReadableStreamDefaultController<Uint8Array>>;
  stopped: string[];
}

function installBackend(sessions: Record<string, ApiSession>): Backend {
  const backend: Backend = { sessions, streams: {}, stopped: [] };
  globalThis.fetch = mock(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/chat/stream') {
      const { sessionId } = JSON.parse(init?.body as string) as { sessionId: string };
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          backend.streams[sessionId] = controller;
        },
      });
      return new Response(body, { status: 200 });
    }
    const stop = /^\/api\/sessions\/([^/]+)\/stop$/.exec(url);
    if (stop) {
      backend.stopped.push(decodeURIComponent(stop[1]));
      return new Response('{}', { status: 200 });
    }
    const get = /^\/api\/sessions\/([^/]+)$/.exec(url);
    if (get) {
      return new Response(JSON.stringify(backend.sessions[decodeURIComponent(get[1])]), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      });
    }
    throw new Error(`unexpected fetch ${url}`);
  }) as unknown as typeof fetch;
  return backend;
}

/** Lets the pending microtasks and stream reads run. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

beforeEach(() => {
  useSessionsStore.setState({ index: {}, order: [], active: null, messages: {} });
  useTurnStore.setState({ turns: {} });
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('turns are keyed by session', () => {
  test('a turn finishing after a session switch lands only in its own session', async () => {
    const backend = installBackend({
      A: session('A', [message('a-u0', 'user', 'earlier')]),
      B: session('B', [message('b-u0', 'user', 'hello')]),
    });

    await useSessionsStore.getState().open('A');
    const turn = useTurnStore.getState().send({ sessionId: 'A', content: 'go' });
    await settle();
    backend.streams.A.enqueue(
      sse('start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );

    await useSessionsStore.getState().open('B');
    const bBefore = useSessionsStore.getState().messages.B;
    expect(useSessionsStore.getState().active?.id).toBe('B');

    backend.streams.A.enqueue(sse('text', { seq: 2, turnId: 't1', chunk: 'reply' }));
    backend.streams.A.enqueue(
      sse('done', {
        seq: 3,
        turnId: 't1',
        sessionId: 'A',
        assistant: message('a-1', 'assistant', 'reply'),
      }),
    );
    backend.streams.A.close();
    await turn;

    const { messages } = useSessionsStore.getState();
    expect(messages.B).toBe(bBefore);
    expect(messages.B.map((m) => m.id)).toEqual(['b-u0']);
    expect(messages.A.map((m) => m.id)).toEqual(['a-u0', 'a-1']);
    expect(useTurnStore.getState().turns.B).toBeUndefined();
    expect(useTurnStore.getState().turns.A.active).toBeNull();
  });

  test('stopping from B does not stop the turn running in A', async () => {
    const backend = installBackend({
      A: session('A', []),
      B: session('B', []),
    });

    await useSessionsStore.getState().open('A');
    const turn = useTurnStore.getState().send({ sessionId: 'A', content: 'go' });
    await settle();
    backend.streams.A.enqueue(
      sse('start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );
    await settle();

    await useSessionsStore.getState().open('B');
    await useTurnStore.getState().cancel('B');

    expect(backend.stopped).toEqual([]);
    expect(useTurnStore.getState().turns.A.active).not.toBeNull();
    expect(useTurnStore.getState().turns.A.stopping).toBe(false);

    backend.streams.A.close();
    await turn;
  });
});
