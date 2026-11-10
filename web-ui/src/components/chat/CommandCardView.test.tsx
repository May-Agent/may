import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { CommandCard } from '../../lib/types';
import { applyEvent, emptyTurn } from '../../lib/stream/reducer';
import { foldTranscript, withToolCard } from '../../lib/transcript';
import { CommandCardView, elapsedText, resultSummary } from './CommandCardView';

// Cards as the daemon serializes them (`command_reading::Card` in the Rust crate).
const tasks: CommandCard = {
  kind: 'tasks',
  changes: [
    {
      change: 'started',
      before: { section: 'Queued', title: 'write docs', detail: 'cover the CLI' },
      after: { section: 'Doing', title: 'write docs', detail: 'cover the CLI' },
    },
  ],
};

const subagent = (state: 'running' | 'done'): CommandCard => ({
  kind: 'subagent',
  agent: 'explorer',
  title: 'Map the parser',
  state,
  startedAt: '2026-11-09T10:00:00Z',
  finishedAt: state === 'done' ? '2026-11-09T10:01:03Z' : null,
  result: state === 'done' ? 'Found two parsers.\nThe old one is dead.' : null,
});

const question = (answered: boolean): CommandCard => ({
  kind: 'askUser',
  requestId: 'r1',
  questions: [
    {
      section: 'db',
      question: 'Which database?',
      options: ['PostgreSQL', 'SQLite'],
      multiSelect: false,
      secret: false,
    },
  ],
  answers: answered ? [{ section: 'db', text: 'SQLite' }] : null,
});

describe('CommandCardView', () => {
  test('a tasks card shows the row before and after', () => {
    const html = renderToStaticMarkup(<CommandCardView card={tasks} />);
    expect(html).toContain('data-change="started"');
    expect(html).toContain('- write docs (Queued) — cover the CLI');
    expect(html).toContain('+ write docs (Doing) — cover the CLI');
  });

  test('a finished subagent shows its time and summary, and keeps the full result', () => {
    const html = renderToStaticMarkup(<CommandCardView card={subagent('done')} />);
    expect(html).toContain('explorer');
    expect(html).toContain('Map the parser');
    expect(html).toContain('1m 03s');
    expect(html).toContain('Found two parsers.…');
    expect(html).toContain('The old one is dead.');
  });

  test('a running subagent has no result yet', () => {
    const html = renderToStaticMarkup(<CommandCardView card={subagent('running')} />);
    expect(html).toContain('data-state="running"');
    expect(html).not.toContain('<details');
  });

  test('an ask_user card shows the options, then the answer', () => {
    const waiting = renderToStaticMarkup(<CommandCardView card={question(false)} />);
    expect(waiting).toContain('Which database?');
    expect(waiting).toContain('PostgreSQL');
    expect(waiting).not.toContain('card-answer');
    const answered = renderToStaticMarkup(<CommandCardView card={question(true)} />);
    expect(answered).toContain('card-answer');
    expect(answered).toContain('SQLite');
  });

  test('time and summary helpers', () => {
    expect(elapsedText(5)).toBe('5s');
    expect(elapsedText(125)).toBe('2m 05s');
    expect(resultSummary('one line')).toBe('one line');
  });
});

describe('cards in the stream and in history', () => {
  test('a live card lands on its call and survives the call ending', () => {
    let state = emptyTurn('t1', 's1', 'a1');
    state = applyEvent(state, {
      kind: 'tool_start',
      seq: 1,
      turnId: 't1',
      id: 'c1',
      name: 'terminal',
      variant: 'terminal',
      arguments: '{}',
      summary: {},
    });
    state = applyEvent(state, { kind: 'tool_card', turnId: '', id: 'c1', card: subagent('running') });
    state = applyEvent(state, {
      kind: 'tool_end',
      seq: 2,
      turnId: 't1',
      id: 'c1',
      name: 'terminal',
      variant: 'terminal',
      result: { kind: 'text', text: 'ok' },
      summary: { script: 'subagent explorer brief.md' },
    });
    expect(state.tools[0]?.summary.card?.kind).toBe('subagent');
    state = applyEvent(state, { kind: 'tool_card', turnId: '', id: 'c1', card: null });
    expect(state.tools[0]?.summary.card).toBeNull();
  });

  test('a stored card comes back with the call in history', () => {
    const messages = foldTranscript([
      { kind: 'user', id: '1', text: 'go' },
      { kind: 'assistant', id: '2', text: 'asking' },
      {
        kind: 'toolCall',
        id: '3',
        callId: 'c1',
        name: 'terminal',
        arguments: '{}',
        status: 'done',
        result: { kind: 'text', text: 'ok' },
        card: question(false),
      },
    ]);
    const events = messages[1]?.toolEvent as { stage: string; summary?: { card?: CommandCard } }[];
    expect(events[0]?.summary?.card?.kind).toBe('askUser');
    const answered = withToolCard(messages, 'c1', question(true));
    const after = answered[1]?.toolEvent as { summary?: { card?: CommandCard } }[];
    expect(after[0]?.summary?.card).toEqual(question(true));
  });

  test('a call without a card keeps the ordinary summary', () => {
    const messages = foldTranscript([
      { kind: 'user', id: '1', text: 'go' },
      { kind: 'assistant', id: '2', text: 'working' },
      { kind: 'toolCall', id: '3', callId: 'c1', name: 'terminal', arguments: '{}', status: 'done' },
    ]);
    const events = messages[1]?.toolEvent as { summary?: unknown }[];
    expect(events[0]?.summary).toBeUndefined();
  });
});
