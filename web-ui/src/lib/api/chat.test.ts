import { afterEach, describe, expect, test } from 'bun:test';
import { stopSession } from './chat';

const realFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = realFetch;
});

describe('stopSession', () => {
  test('deletes the current turn, the route the daemon serves', async () => {
    const requests: { url: string; method: string | undefined }[] = [];
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      requests.push({ url: String(input), method: init?.method });
      return new Response(null, { status: 204 });
    }) as typeof fetch;

    await stopSession('s 1');

    expect(requests).toEqual([
      { url: '/api/sessions/s%201/turns/current', method: 'DELETE' },
    ]);
  });
});
