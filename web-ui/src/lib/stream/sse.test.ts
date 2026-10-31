import { describe, expect, test } from 'bun:test';
import { parseNamedSseStream } from './sse';

function response(text: string): Response {
  return new Response(new TextEncoder().encode(text));
}

describe('parseNamedSseStream', () => {
  test('keeps the id of each frame', async () => {
    const events = [];
    for await (const event of parseNamedSseStream<{ n: number }>(
      response('event: session_turn\nid: 7\ndata: {"n":1}\n\nevent: heartbeat\ndata: {"n":2}\n\n'),
    )) {
      events.push(event);
    }
    expect(events).toEqual([
      { event: 'session_turn', data: { n: 1 }, id: '7' },
      { event: 'heartbeat', data: { n: 2 }, id: undefined },
    ]);
  });
});
