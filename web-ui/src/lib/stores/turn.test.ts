import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import type { ApiMessage, ApiSession } from '../types';
import { useSessionsStore } from './sessions';
import { useTurnStore } from './turn';
import { chatRows } from '../chat/rows';

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

/** One `session_turn` frame of `/api/events`. */
function frame(sessionId: string, event: string, data: Record<string, unknown>): Uint8Array {
  const payload = { sessionId, event, data };
  return encoder.encode(`event: session_turn\ndata: ${JSON.stringify(payload)}\n\n`);
}

interface Backend {
  sessions: Record<string, ApiSession>;
  /** Controllers of open turn streams, by session id. */
  streams: Record<string, ReadableStreamDefaultController<Uint8Array>>;
  sent: string[];
  stopped: string[];
}

function installBackend(sessions: Record<string, ApiSession>): Backend {
  const backend: Backend = { sessions, streams: {}, sent: [], stopped: [] };
  globalThis.fetch = mock(async (input: RequestInfo | URL) => {
    const url = String(input);
    const events = /^\/api\/sessions\/([^/]+)\/events/.exec(url);
    if (events) {
      const sessionId = decodeURIComponent(events[1]);
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          backend.streams[sessionId] = controller;
        },
      });
      return new Response(body, { status: 200 });
    }
    const background = /^\/api\/sessions\/([^/]+)\/background$/.exec(url);
    if (background) {
      const sessionId = decodeURIComponent(background[1]);
      return new Response(
        JSON.stringify({ sessionId, jobs: [], subagents: [] }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      );
    }
    const send = /^\/api\/sessions\/([^/]+)\/messages$/.exec(url);
    if (send) {
      backend.sent.push(decodeURIComponent(send[1]));
      return new Response('{}', { status: 200 });
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

const unwatchers: Array<() => void> = [];

beforeEach(() => {
  useSessionsStore.setState({ index: {}, order: [], active: null, messages: {} });
  useTurnStore.setState({ turns: {} });
});

afterEach(() => {
  for (const unwatch of unwatchers.splice(0)) unwatch();
  globalThis.fetch = originalFetch;
});

describe('turns are keyed by session', () => {
  test('a turn finishing after a session switch lands only in its own session', async () => {
    const backend = installBackend({
      A: session('A', [message('a-u0', 'user', 'earlier')]),
      B: session('B', [message('b-u0', 'user', 'hello')]),
    });

    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    await useTurnStore.getState().send({ sessionId: 'A', content: 'go' });
    expect(backend.sent).toEqual(['A']);
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );

    await useSessionsStore.getState().open('B');
    const bBefore = useSessionsStore.getState().messages.B;
    expect(useSessionsStore.getState().active?.id).toBe('B');

    backend.streams.A.enqueue(frame('A', 'text', { seq: 2, turnId: 't1', chunk: 'reply' }));
    backend.streams.A.enqueue(
      frame('A', 'done', {
        seq: 3,
        turnId: 't1',
        sessionId: 'A',
        userMessageId: null,
        messageIds: ['a-1'],
        assistant: message('a-1', 'assistant', 'reply'),
      }),
    );
    backend.streams.A.enqueue(
      frame('A', 'end', { seq: 4, turnId: 't1', outcome: 'completed' }),
    );
    await settle();

    const { messages } = useSessionsStore.getState();
    expect(messages.B).toBe(bBefore);
    expect(messages.B.map((m) => m.id)).toEqual(['b-u0']);
    expect(messages.A.map((m) => m.id)).toEqual(['a-u0', 'a-1']);
    expect(useTurnStore.getState().turns.B).toBeUndefined();
    expect(useTurnStore.getState().turns.A.active).toBeNull();
  });

  test('a turn another client started streams into the watching view', async () => {
    const backend = installBackend({ A: session('A', []) });

    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 1, turnId: 't9', sessionId: 'A', assistantId: 'a-9' }),
    );
    backend.streams.A.enqueue(frame('A', 'text', { seq: 2, turnId: 't9', chunk: 'from tui' }));
    await settle();

    expect(backend.sent).toEqual([]);
    expect(useTurnStore.getState().turns.A.active?.text).toBe('from tui');
  });

  test('stopping from B does not stop the turn running in A', async () => {
    const backend = installBackend({
      A: session('A', []),
      B: session('B', []),
    });

    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    await useTurnStore.getState().send({ sessionId: 'A', content: 'go' });
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );
    await settle();

    await useSessionsStore.getState().open('B');
    await useTurnStore.getState().cancel('B');

    expect(backend.stopped).toEqual([]);
    expect(useTurnStore.getState().turns.A.active).not.toBeNull();
    expect(useTurnStore.getState().turns.A.stopping).toBe(false);
  });
});

describe('a withdrawn turn', () => {
  const text = '你好 こんにちは 안녕하세요';
  const withdrawn = (sessionId: string) => [
    frame(sessionId, 'start', { seq: 1, turnId: 't1', sessionId, assistantId: 'a-1' }),
    frame(sessionId, 'withdrawn', { seq: 2, turnId: 't1', content: text, attachments: [] }),
    frame(sessionId, 'end', { seq: 3, turnId: 't1', outcome: 'cancelled' }),
  ];

  test('leaves the conversation and returns to the sender composer', async () => {
    const backend = installBackend({ A: session('A', [message('a-u0', 'user', 'earlier')]) });
    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    useSessionsStore.getState().appendMessage('A', message('local-1', 'user', text));
    await useTurnStore.getState().send({ sessionId: 'A', content: text });
    for (const bytes of withdrawn('A')) backend.streams.A.enqueue(bytes);
    await settle();

    expect(useSessionsStore.getState().messages.A.map((m) => m.id)).toEqual(['a-u0']);
    const turn = useTurnStore.getState().turns.A;
    expect(turn.active).toBeNull();
    expect(turn.last).toBeNull();
    expect(turn.restored?.content).toBe(text);
  });

  test('is removed without a draft in a client that did not send it', async () => {
    const backend = installBackend({ A: session('A', [message('a-u0', 'user', text)]) });
    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    for (const bytes of withdrawn('A')) backend.streams.A.enqueue(bytes);
    await settle();

    expect(useSessionsStore.getState().messages.A).toEqual([]);
    expect(useTurnStore.getState().turns.A.restored).toBeNull();
  });
});

describe('a finished turn keeps its components through a refresh', () => {
  test('streamed messages take their stored ids, so the refreshed list reuses their rows', async () => {
    const stored = [
      { ...message('msg-7', 'user', '你好 こんにちは 안녕하세요'), attachments: [] },
      message('msg-8', 'assistant', '我先查一下 調べます 확인할게요'),
      message('msg-10', 'assistant', '完成 完了 완료'),
    ];
    const backend = installBackend({ A: session('A', []) });

    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    useSessionsStore.getState().appendMessage('A', {
      ...message('local-1', 'user', stored[0]!.content),
      attachments: [],
    });
    await useTurnStore.getState().send({ sessionId: 'A', content: stored[0]!.content });
    const frames: Array<[string, Record<string, unknown>]> = [
      ['start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'assistant-x' }],
      ['text', { seq: 2, turnId: 't1', chunk: stored[1]!.content }],
      [
        'done',
        {
          seq: 3,
          turnId: 't1',
          sessionId: 'A',
          userMessageId: 'msg-7',
          messageIds: ['msg-8', 'msg-10'],
          assistant: stored[1],
        },
      ],
      ['end', { seq: 4, turnId: 't1', outcome: 'completed' }],
    ];
    for (const [event, data] of frames) backend.streams.A.enqueue(frame('A', event, data));
    await settle();

    const rowsOf = () =>
      chatRows(useSessionsStore.getState().messages.A, useTurnStore.getState().turns.A.last).map(
        ({ key, kind }) => ({ key, kind }),
      );
    const before = rowsOf();
    expect(before).toEqual([
      { key: 'msg-7', kind: 'user' },
      { key: 'msg-8', kind: 'turn' },
    ]);

    // The refresh the session's changed `updated_at` triggers.
    backend.sessions.A = session('A', stored);
    await useSessionsStore.getState().refreshActive('A');

    expect(rowsOf()).toEqual(before);
  });

  test('a pause before any reply settles the user message and adds no assistant message', async () => {
    const backend = installBackend({ A: session('A', []) });
    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    useSessionsStore.getState().appendMessage('A', message('local-1', 'user', '你好 こんにちは 안녕하세요'));
    await useTurnStore.getState().send({ sessionId: 'A', content: '你好' });
    const frames: Array<[string, Record<string, unknown>]> = [
      ['start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'assistant-x' }],
      [
        'paused',
        {
          seq: 2,
          turnId: 't1',
          sessionId: 'A',
          userMessageId: 'msg-7',
          messageIds: [],
          assistant: null,
        },
      ],
      ['end', { seq: 3, turnId: 't1', outcome: 'paused' }],
    ];
    for (const [event, data] of frames) backend.streams.A.enqueue(frame('A', event, data));
    await settle();

    expect(useSessionsStore.getState().messages.A.map((m) => m.id)).toEqual(['msg-7']);
    expect(useTurnStore.getState().turns.A.active).toBeNull();
  });
});

describe('the session stream', () => {
  const event = (kind: string, data: Record<string, unknown>, id?: string) =>
    encoder.encode(
      `${id ? `id: ${id}\n` : ''}event: ${kind}\ndata: ${JSON.stringify(data)}\n\n`,
    );

  test('a reset drops the open turn and reloads the session', async () => {
    const backend = installBackend({ A: session('A', []) });
    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );
    await settle();
    expect(useTurnStore.getState().turns.A.active).not.toBeNull();

    backend.sessions.A = session('A', [message('u-1', 'user', 'later')]);
    backend.streams.A.enqueue(event('reset', { sessionId: 'A' }, '9.1'));
    await settle();

    expect(useTurnStore.getState().turns.A.active).toBeNull();
    expect(useSessionsStore.getState().messages.A.map((m) => m.id)).toEqual(['u-1']);
  });

  test('a turn whose prompt is already in history is not rendered twice', async () => {
    const backend = installBackend({
      A: session('A', [message('msg-7', 'user', 'earlier'), message('a-1', 'assistant', 'done')]),
    });
    await useSessionsStore.getState().open('A');
    unwatchers.push(useTurnStore.getState().watch('A'));
    await settle();

    // The turn ended between the session read's cursor and its messages
    // arriving: its frames replay on the stream.
    backend.streams.A.enqueue(
      event('session_agent', {
        sessionId: 'A',
        turnId: 't1',
        at: '2026-01-01T00:00:00Z',
        body: { kind: 'start', messageId: 7 },
      }),
    );
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 1, turnId: 't1', sessionId: 'A', assistantId: 'a-1' }),
    );
    backend.streams.A.enqueue(frame('A', 'text', { seq: 2, turnId: 't1', chunk: 'dup' }));
    backend.streams.A.enqueue(
      frame('A', 'end', { seq: 3, turnId: 't1', outcome: 'completed' }),
    );
    await settle();

    expect(useTurnStore.getState().turns.A?.active ?? null).toBeNull();
    expect(useSessionsStore.getState().messages.A.map((m) => m.id)).toEqual(['msg-7', 'a-1']);

    // A turn that is not in history still streams normally.
    backend.streams.A.enqueue(
      event('session_agent', {
        sessionId: 'A',
        turnId: 't2',
        at: '2026-01-01T00:00:00Z',
        body: { kind: 'start', messageId: 9 },
      }),
    );
    backend.streams.A.enqueue(
      frame('A', 'start', { seq: 4, turnId: 't2', sessionId: 'A', assistantId: 'a-2' }),
    );
    await settle();
    expect(useTurnStore.getState().turns.A.active).not.toBeNull();
  });
});
