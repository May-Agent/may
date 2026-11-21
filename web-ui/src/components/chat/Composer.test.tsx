import { afterEach, describe, expect, mock, test } from 'bun:test';
import type { ApiSession } from '@/lib/types';
import { useSessionsStore } from '@/lib/stores/sessions';
import { fileToAttachment } from './composerAttachments';

const originalFetch = globalThis.fetch;

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function apiSession(id: string): ApiSession {
  return {
    id,
    title: 'New Chat',
    lastModified: '2026-01-01T00:00:00Z',
    preview: '',
    profileId: 'p1',
    model: 'provider::model',
    reasoningEffort: null,
    runtime: {},
  };
}

function attachmentRecord(id: string, name: string) {
  return { id, name, type: 'image', url: `/api/sessions/real-session/files/${name}` };
}

function reset() {
  useSessionsStore.setState({
    index: {},
    order: [],
    active: null,
    drafts: {},
    suspended: {},
    messages: {},
    loading: false,
    error: null,
  });
}

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('fileToAttachment', () => {
  test('a draft session materializes first, then uploads to the real id', async () => {
    reset();
    const draft = await useSessionsStore.getState().createNew('p1');
    const urls: string[] = [];
    const methods: string[] = [];
    globalThis.fetch = mock((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      urls.push(url);
      methods.push(init?.method ?? 'GET');
      return Promise.resolve(
        json(url === '/api/sessions' ? apiSession('real-session') : attachmentRecord('a1', 'shot.png')),
      );
    }) as unknown as typeof fetch;

    const pending = { current: null };
    const record = await fileToAttachment(
      draft.id,
      new File(['PNG'], 'shot.png', { type: 'image/png' }),
      pending,
    );

    // The draft became a daemon session before the attachment went up —
    // never a 404 on `draft_…`.
    expect(methods).toEqual(['POST', 'POST']);
    expect(urls[0]).toBe('/api/sessions');
    expect(urls[1]).toBe('/api/sessions/real-session/attachments?name=shot.png');
    expect(record.id).toBe('a1');
    expect(useSessionsStore.getState().active?.id).toBe('real-session');
  });

  test('a real session uploads directly with no session create', async () => {
    reset();
    const urls: string[] = [];
    globalThis.fetch = mock((input: RequestInfo | URL) => {
      urls.push(String(input));
      return Promise.resolve(json(attachmentRecord('a2', 'f.png')));
    }) as unknown as typeof fetch;

    const pending = { current: null };
    await fileToAttachment(
      'real-session',
      new File(['PNG'], 'f.png', { type: 'image/png' }),
      pending,
    );

    expect(urls).toEqual(['/api/sessions/real-session/attachments?name=f.png']);
  });

  test('concurrent uploads on one draft share a single materialization', async () => {
    reset();
    const draft = await useSessionsStore.getState().createNew('p1');
    const urls: string[] = [];
    globalThis.fetch = mock((input: RequestInfo | URL) => {
      const url = String(input);
      urls.push(url);
      return Promise.resolve(
        json(url === '/api/sessions' ? apiSession('real-session') : attachmentRecord('a3', 'f.png')),
      );
    }) as unknown as typeof fetch;

    const pending = { current: null };
    await Promise.all([
      fileToAttachment(draft.id, new File(['1'], 'a.png', { type: 'image/png' }), pending),
      fileToAttachment(draft.id, new File(['2'], 'b.png', { type: 'image/png' }), pending),
    ]);

    expect(urls.filter((u) => u === '/api/sessions')).toHaveLength(1);
    expect(urls.filter((u) => u.startsWith('/api/sessions/real-session/attachments'))).toHaveLength(2);
  });
});
