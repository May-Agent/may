import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import type { ApiMessage, ApiSession } from '../types';
import { isDraftSessionId, useSessionsStore } from './sessions';

const originalFetch = globalThis.fetch;

interface Calls {
  urls: string[];
  methods: string[];
  bodies: string[];
}

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function apiSession(id: string, over: Partial<ApiSession> = {}): ApiSession {
  return {
    id,
    title: 'New Chat',
    lastModified: '2026-01-01T00:00:00Z',
    preview: '',
    messages: [],
    profileId: 'p1',
    model: 'provider::model',
    reasoningEffort: null,
    runtime: {},
    ...over,
  };
}

function reset() {
  useSessionsStore.setState({
    index: {},
    order: [],
    active: null,
    drafts: {},
    messages: {},
    loading: false,
    error: null,
  });
}

describe('draft sessions', () => {
  let calls: Calls;

  beforeEach(() => {
    reset();
    calls = { urls: [], methods: [], bodies: [] };
    globalThis.fetch = mock((input: RequestInfo | URL, init?: RequestInit) => {
      calls.urls.push(String(input));
      calls.methods.push(init?.method ?? 'GET');
      if (init?.body) calls.bodies.push(String(init.body));
      return Promise.resolve(json(apiSession('real-session-id')));
    }) as unknown as typeof fetch;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  test('createNew opens a local draft without a session request', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');

    expect(isDraftSessionId(draft.id)).toBe(true);
    expect(useSessionsStore.getState().drafts[draft.id]?.session.id).toBe(draft.id);
    expect(useSessionsStore.getState().active?.id).toBe(draft.id);
    expect(useSessionsStore.getState().messages[draft.id]).toEqual([]);
    // Nothing reached the daemon — and the draft is not listed as a session.
    expect(calls.urls).toEqual([]);
    expect(useSessionsStore.getState().order).not.toContain(draft.id);
  });

  test('open resolves a draft locally', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');
    useSessionsStore.setState({ active: null });

    await useSessionsStore.getState().open(draft.id);

    expect(useSessionsStore.getState().active?.id).toBe(draft.id);
    expect(calls.urls).toEqual([]);
  });

  test('materialize creates the session and rekeys the stores', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');
    const session = await useSessionsStore.getState().materialize(draft.id);

    expect(calls.methods).toEqual(['POST']);
    expect(calls.urls[0]).toBe('/api/sessions');
    expect(calls.bodies[0]).toContain('"profileId":"p1"');
    expect(useSessionsStore.getState().drafts[draft.id]).toBeUndefined();
    expect(useSessionsStore.getState().active?.id).toBe(session.id);
    expect(useSessionsStore.getState().index[session.id]?.id).toBe(session.id);
    expect(useSessionsStore.getState().order[0]).toBe(session.id);
    expect(useSessionsStore.getState().messages[session.id]).toEqual([]);
  });

  test('draft settings apply at materialize', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');
    const store = useSessionsStore.getState();
    await store.rename(draft.id, 'Renamed');
    await store.setModel(draft.id, 'provider::other');
    await store.setReasoningEffort(draft.id, 'high');
    // Staged edits stayed local.
    expect(calls.urls).toEqual([]);
    expect(useSessionsStore.getState().active?.title).toBe('Renamed');
    expect(useSessionsStore.getState().active?.model).toBe('provider::other');

    await useSessionsStore.getState().materialize(draft.id);

    expect(calls.methods).toEqual(['POST', 'PUT']);
    expect(calls.bodies[0]).toContain('"model":"provider::other"');
    expect(calls.bodies[1]).toContain('"title":"Renamed"');
    expect(calls.bodies[1]).toContain('"reasoningEffort":"high"');
  });

  test('a staged plan materializes with the session', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');
    useSessionsStore.getState().saveDraftPlan(draft.id, '# Plan', 'draft');

    await useSessionsStore.getState().materialize(draft.id);

    expect(calls.methods).toEqual(['POST', 'PUT']);
    expect(calls.urls[1]).toBe('/api/sessions/real-session-id/tasks');
    expect(calls.bodies[1]).toContain('"content":"# Plan"');
    expect(calls.bodies[1]).toContain('"planState":"draft"');
  });

  test('remove drops a draft without a delete request', async () => {
    const draft = await useSessionsStore.getState().createNew('p1');
    await useSessionsStore.getState().remove(draft.id);

    expect(useSessionsStore.getState().drafts[draft.id]).toBeUndefined();
    expect(useSessionsStore.getState().active).toBeNull();
    expect(calls.urls).toEqual([]);
  });
});

describe('optimistic sends', () => {
  const message = (id: string, content: string): ApiMessage => ({
    id,
    role: 'user',
    content,
    timestamp: '2026-01-01T00:00:00Z',
    attachments: [],
  });

  function serve(messages: ApiMessage[]) {
    globalThis.fetch = mock(() =>
      Promise.resolve(json(apiSession('s1', { messages }))),
    ) as unknown as typeof fetch;
  }

  beforeEach(reset);
  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  test('refreshActive keeps a sent message the daemon has not committed', async () => {
    useSessionsStore.setState({
      active: apiSession('s1'),
      messages: { s1: [message('local-1', 'hello')] },
    });
    serve([]);

    await useSessionsStore.getState().refreshActive('s1');

    expect(useSessionsStore.getState().messages.s1?.map((m) => m.id)).toEqual(['local-1']);
  });

  test('refreshActive drops the local echo once its row is persisted', async () => {
    useSessionsStore.setState({
      active: apiSession('s1'),
      messages: { s1: [message('local-1', 'hello')] },
    });
    serve([message('m1', 'hello')]);

    await useSessionsStore.getState().refreshActive('s1');

    expect(useSessionsStore.getState().messages.s1?.map((m) => m.id)).toEqual(['m1']);
  });
});
