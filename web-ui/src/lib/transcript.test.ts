import { describe, expect, test } from 'bun:test';
import { foldTranscript, type TranscriptEntry } from './transcript';

describe('foldTranscript', () => {
  const entry = (over: Partial<TranscriptEntry>): TranscriptEntry => ({
    kind: 'user', id: '0', ...over,
  });

  test('folds a turn the way map_db_messages did', () => {
    const entries: TranscriptEntry[] = [
      entry({ kind: 'hiddenContext', id: '1', text: 'ctx' }),
      entry({ kind: 'user', id: '2', text: 'hi', attachments: [{ id: 'a1', name: 'p.png', type: 'image', url: '/api/sessions/s/files/1.png' }] }),
      entry({ kind: 'toolCall', id: '3', callId: 'c1', name: 'terminal', arguments: '{"cmd":"ls"}', status: 'done', result: { kind: 'text', text: 'ok' } }),
      entry({ kind: 'interactionRequest', id: '4', interaction: 'askUser', content: '{}' }),
      entry({ kind: 'assistant', id: '5', text: 'answer', thinking: 'hmm', stats: { promptTokens: 1 } }),
      entry({ kind: 'toolCall', id: '6', callId: 'c2', name: 'web_search', arguments: '{}', status: 'done', result: { kind: 'text', text: 'r' } }),
      entry({ kind: 'toolCall', id: '7', callId: 'c3', name: 'monitor', arguments: '{}', status: 'running' }),
    ];
    const msgs = foldTranscript(entries);
    expect(msgs).toHaveLength(2);
    expect(msgs[0]).toMatchObject({ id: 'msg-2', role: 'user', content: 'hi' });
    expect(msgs[0].attachments).toHaveLength(1);
    expect(msgs[1]).toMatchObject({ id: 'msg-5', role: 'assistant', content: 'answer', thinking: 'hmm' });
    const events = msgs[1].toolEvent as Record<string, unknown>[];
    expect(events).toHaveLength(5);
    expect(events[0]).toMatchObject({ stage: 'start', id: 'c1', name: 'terminal', variant: 'terminal', arguments: '{"cmd":"ls"}', seq: 0 });
    expect(events[1]).toMatchObject({ stage: 'end', id: 'c1', variant: 'terminal', result: { kind: 'text', text: 'ok' }, seq: 1 });
    expect(events[2]).toMatchObject({ stage: 'start', id: 'c2', variant: 'web_search', seq: 0 });
    expect(events[3]).toMatchObject({ stage: 'end', id: 'c2', seq: 1 });
    expect(events[4]).toMatchObject({ stage: 'start', id: 'c3', variant: 'generic', seq: 2 });
  });

  test('tools before the first assistant of a turn attach to it', () => {
    const msgs = foldTranscript([
      entry({ kind: 'user', id: '1', text: 'q' }),
      entry({ kind: 'toolCall', id: '2', callId: 'c', name: 'task', arguments: '{}', status: 'running' }),
      entry({ kind: 'assistant', id: '3', text: 'a' }),
    ]);
    expect((msgs[1].toolEvent as unknown[]).length).toBe(1);
  });
});
