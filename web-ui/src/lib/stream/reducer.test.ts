import { describe, expect, test } from 'bun:test';
import { applyEvent, emptyTurn, reduceEvents } from './reducer';
import type { ChatStreamEvent } from '../types';
import { projectField } from './partialJson';

function stream(events: ChatStreamEvent[]) {
  return reduceEvents(events);
}

describe('reducer', () => {
  test('start initializes fresh turn', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
    ]);
    expect(t.turnId).toBe('t1');
    expect(t.sessionId).toBe('s1');
    expect(t.assistantId).toBe('a1');
    expect(t.status).toBe('streaming');
    expect(t.lastSeq).toBe(1);
  });

  test('text chunks concatenate in order', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'Hello ' },
      { kind: 'text', seq: 3, turnId: 't1', chunk: 'world' },
    ]);
    expect(t.text).toBe('Hello world');
  });

  test('out-of-order event with stale seq is dropped', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 5, turnId: 't1', chunk: 'B' },
      { kind: 'text', seq: 3, turnId: 't1', chunk: 'A' },
    ]);
    expect(t.text).toBe('B');
    expect(t.lastSeq).toBe(5);
  });

  test('tool_start → tool_delta → tool_end builds monotonic argumentsRaw', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      {
        kind: 'tool_start',
        seq: 2,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        arguments: '{"command":"',
      },
      {
        kind: 'tool_delta',
        seq: 3,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        argumentsFragment: 'npm run',
      },
      {
        kind: 'tool_delta',
        seq: 4,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        argumentsFragment: ' build","description":"build prod"}',
      },
      {
        kind: 'tool_end',
        seq: 5,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        arguments: '{"command":"npm run build","description":"build prod"}',
        result: { exitCode: 0 },
      },
    ]);
    expect(t.tools.length).toBe(1);
    const tool = t.tools[0]!;
    expect(tool.status).toBe('done');
    expect(tool.argumentsRaw).toBe(
      '{"command":"npm run build","description":"build prod"}',
    );
    expect(tool.result).toEqual({ exitCode: 0 });
    expect(projectField(tool.argumentsRaw, 'command')).toBe('npm run build');
    expect(projectField(tool.argumentsRaw, 'description')).toBe('build prod');
  });

  test('command field is readable every frame of stream', () => {
    // This is THE test for "streaming tool calls feel mature" — the user
    // should see the command text appear character by character.
    const events: ChatStreamEvent[] = [
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      {
        kind: 'tool_start',
        seq: 2,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        arguments: '{"command":"',
      },
    ];

    let state = emptyTurn('', '', '');
    const frames: string[] = [];
    const fragments = ['g', 'i', 't', ' ', 's', 't', 'a', 't', 'u', 's', '"'];
    let seq = 2;
    state = applyEvent(state, events[0]!);
    state = applyEvent(state, events[1]!);
    frames.push(projectField(state.tools[0]!.argumentsRaw, 'command'));

    for (const frag of fragments) {
      seq += 1;
      state = applyEvent(state, {
        kind: 'tool_delta',
        seq,
        turnId: 't1',
        id: 'c1',
        name: 'terminal',
        variant: 'terminal',
        argumentsFragment: frag,
      });
      frames.push(projectField(state.tools[0]!.argumentsRaw, 'command'));
    }

    expect(frames[0]).toBe('');
    expect(frames[1]).toBe('g');
    expect(frames[2]).toBe('gi');
    expect(frames[frames.length - 1]).toBe('git status');
    // Monotonic growth.
    for (let i = 1; i < frames.length; i++) {
      expect(frames[i]!.startsWith(frames[i - 1]!)).toBe(true);
    }
  });

  test('todo_update replaces the plan snapshot', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      {
        kind: 'todo_update',
        seq: 2,
        turnId: 't1',
        items: [
          { text: 'step 1', checked: true },
          { text: 'step 2', checked: false },
        ],
        total: 2,
        completed: 1,
      },
    ]);
    expect(t.todo?.total).toBe(2);
    expect(t.todo?.completed).toBe(1);
    expect(t.todo?.items[0]?.checked).toBe(true);
  });

  test('error marks turn done and preserves retryable flag', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'partial' },
      {
        kind: 'error',
        seq: 3,
        turnId: 't1',
        code: 'provider_unavailable',
        message: 'upstream 503',
        retryable: true,
      },
    ]);
    expect(t.status).toBe('done');
    expect(t.error?.code).toBe('provider_unavailable');
    expect(t.error?.retryable).toBe(true);
    // Text up to the error is preserved.
    expect(t.text).toBe('partial');
  });

  test('done event finalizes text with canonical assistant content', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'Hel' },
      {
        kind: 'done',
        seq: 3,
        turnId: 't1',
        sessionId: 's1',
        assistant: {
          id: 'a1',
          role: 'assistant',
          content: 'Hello, world!',
          timestamp: '2026-04-20T00:00:00Z',
        },
      },
    ]);
    expect(t.status).toBe('done');
    expect(t.text).toBe('Hello, world!');
  });

  test('segments preserve chronological order of text and tool events', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'Let me search. ' },
      {
        kind: 'tool_start',
        seq: 3,
        turnId: 't1',
        id: 'c1',
        name: 'web_search',
        variant: 'web_search',
        arguments: '{"q":"may"}',
      },
      {
        kind: 'tool_end',
        seq: 4,
        turnId: 't1',
        id: 'c1',
        name: 'web_search',
        variant: 'web_search',
        arguments: '{"q":"may"}',
        result: {},
      },
      { kind: 'text', seq: 5, turnId: 't1', chunk: 'Found it. ' },
      {
        kind: 'tool_start',
        seq: 6,
        turnId: 't1',
        id: 'c2',
        name: 'web_fetch',
        variant: 'web_fetch',
        arguments: '{"url":"x"}',
      },
      { kind: 'text', seq: 7, turnId: 't1', chunk: 'Done.' },
    ]);
    const kinds = t.segments.map((s) => (s.kind === 'text' ? 'text' : s.toolId));
    expect(kinds).toEqual(['text', 'c1', 'text', 'c2', 'text']);
    expect(t.segments[0]).toMatchObject({ kind: 'text', text: 'Let me search. ' });
    expect(t.segments[2]).toMatchObject({ kind: 'text', text: 'Found it. ' });
    expect(t.segments[4]).toMatchObject({ kind: 'text', text: 'Done.' });
    expect(t.text).toBe('Let me search. Found it. Done.');
  });

  test('consecutive text chunks coalesce into one segment', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'Hel' },
      { kind: 'text', seq: 3, turnId: 't1', chunk: 'lo ' },
      { kind: 'text', seq: 4, turnId: 't1', chunk: 'world' },
    ]);
    expect(t.segments.length).toBe(1);
    expect(t.segments[0]).toMatchObject({ kind: 'text', text: 'Hello world' });
  });

  test('ping bumps seq but leaves state otherwise untouched', () => {
    const t = stream([
      { kind: 'start', seq: 1, turnId: 't1', sessionId: 's1', assistantId: 'a1' },
      { kind: 'text', seq: 2, turnId: 't1', chunk: 'x' },
      { kind: 'ping', seq: 3, turnId: 't1' },
    ]);
    expect(t.text).toBe('x');
    expect(t.lastSeq).toBe(3);
    expect(t.status).toBe('streaming');
  });
});
