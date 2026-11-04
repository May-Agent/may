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

const chose = (...values: string[]): SectionAnswer => ({
  selection: new Set(values),
  custom: '',
  declined: false,
});
const typed = (text: string): SectionAnswer => ({ selection: new Set(), custom: text, declined: false });
const declined: SectionAnswer = { selection: new Set(), custom: '', declined: true };

const secretRequest: PersistedAskUserRequest = {
  id: 'req-2',
  questions: [
    {
      section: 'DEPLOY_TOKEN',
      question: 'Deploy token?',
      option: [],
      multiSelect: false,
      secret: { name: 'DEPLOY_TOKEN', scope: 'session' },
    },
  ],
};

describe('buildAnswers for a secret question', () => {
  test('the typed value is the answer, and an empty field submits nothing', () => {
    expect(buildAnswers(secretRequest, new Map())).toBeNull();
    expect(buildAnswers(secretRequest, new Map([['DEPLOY_TOKEN', typed('  ')]]))).toBeNull();
    expect(buildAnswers(secretRequest, new Map([['DEPLOY_TOKEN', typed(' s3cret ')]]))).toEqual([
      { section: 'DEPLOY_TOKEN', answer: { kind: 'custom', text: 's3cret' } },
    ]);
  });

  test('declining is its own answer and wins over the field', () => {
    expect(buildAnswers(secretRequest, new Map([['DEPLOY_TOKEN', declined]]))).toEqual([
      { section: 'DEPLOY_TOKEN', answer: { kind: 'declined' } },
    ]);
    expect(
      buildAnswers(secretRequest, new Map([['DEPLOY_TOKEN', { ...typed('x'), declined: true }]])),
    ).toEqual([{ section: 'DEPLOY_TOKEN', answer: { kind: 'declined' } }]);
  });

  test('a plain question cannot be declined', () => {
    expect(buildAnswers(request, new Map([['db', declined], ['notes', typed('x')]]))).toBeNull();
  });
});

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
