import { afterEach, describe, expect, test } from 'bun:test';
import { openSessionTurns } from './chat';

const realFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = realFetch;
});

function serve(body: string, seen: string[]) {
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    seen.push(String(input));
    return new Response(new TextEncoder().encode(body), { status: 200 });
  }) as typeof fetch;
}

describe('openSessionTurns', () => {
  test('a reopened stream names the last frame it saw', async () => {
    const urls: string[] = [];
    serve('', urls);
    await openSessionTurns('s1', undefined, { after: '41' });
    expect(urls[0]).toContain('after=41');
  });

  test('reports frame ids and a resync, and yields the turn events', async () => {
    const urls: string[] = [];
    serve(
      'event: session_resync\ndata: {"sessionId":"s1"}\n\n' +
        'event: session_turn\nid: 9\ndata: {"sessionId":"s1","event":"start","data":{"seq":1}}\n\n',
      urls,
    );
    const ids: string[] = [];
    let resyncs = 0;
    const stream = await openSessionTurns('s1', undefined, {
      onFrameId: (id) => ids.push(id),
      onResync: () => {
        resyncs += 1;
      },
    });
    const kinds = [];
    for await (const event of stream.events) kinds.push(event.kind);
    expect(urls[0]).not.toContain('after=');
    expect(resyncs).toBe(1);
    expect(ids).toEqual(['9']);
    expect(kinds).toEqual(['start']);
  });
});
