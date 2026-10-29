import { describe, expect, test } from 'bun:test';
import { buildAnswers, type SectionAnswer } from './askUserAnswers';
import type { PersistedAskUserRequest } from './types';

const request: PersistedAskUserRequest = {
  id: 'req-1',
  questions: [
    { section: 'db', question: 'Which database?', option: ['PostgreSQL', 'SQLite'], multiSelect: false },
    { section: 'notes', question: 'Anything else?', option: [], multiSelect: false },
  ],
};

const chose = (...values: string[]): SectionAnswer => ({ selection: new Set(values), custom: '' });
const typed = (text: string): SectionAnswer => ({ selection: new Set(), custom: text });

describe('buildAnswers', () => {
  test('an empty form submits nothing', () => {
    expect(buildAnswers(request, new Map())).toBeNull();
  });

  test('a form with an unanswered section submits nothing', () => {
    expect(buildAnswers(request, new Map([['db', chose('SQLite')]]))).toBeNull();
    expect(buildAnswers(request, new Map([['db', chose('SQLite')], ['notes', typed('   ')]]))).toBeNull();
  });

  test('every section answered submits the choices and the text', () => {
    expect(
      buildAnswers(request, new Map([['db', chose('SQLite')], ['notes', typed(' keep it small ')]])),
    ).toEqual([
      { section: 'db', answer: { kind: 'selected', values: ['SQLite'] } },
      { section: 'notes', answer: { kind: 'custom', text: 'keep it small' } },
    ]);
  });

  test('there is no skipped answer', () => {
    const answers = buildAnswers(request, new Map([['db', chose('SQLite')], ['notes', typed('x')]]));
    expect(JSON.stringify(answers)).not.toContain('skip');
  });
});
