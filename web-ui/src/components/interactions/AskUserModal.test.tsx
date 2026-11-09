import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { PersistedAskUserRequest, TaskRow } from '@/lib/types';
import { OpenQuestions } from './AskUserModal';

const row = (title: string, blocking: string | null): TaskRow => ({
  section: 'doing',
  title,
  detail: '',
  blocking,
  references: [],
});

const request = (id: string, question: string): PersistedAskUserRequest => ({
  id,
  questions: [{ section: 'db', question, option: ['PostgreSQL', 'SQLite'], multiSelect: false }],
});

describe('OpenQuestions', () => {
  test('lists, under each question, the rows whose BLOCKING mark names it', () => {
    const markup = renderToStaticMarkup(
      <OpenQuestions
        requests={[request('q1', '选哪个数据库?'), request('q2', 'Anything else?')]}
        rows={[
          row('迁移数据库', 'q1'),
          row('データベース設計 스키마', 'q1'),
          row('unrelated', 'q9'),
          row('free text reason', 'waiting on review'),
        ]}
      />,
    );
    const first = markup.slice(markup.indexOf('data-request-id="q1"'), markup.indexOf('data-request-id="q2"'));
    const second = markup.slice(markup.indexOf('data-request-id="q2"'));
    expect(first).toContain('迁移数据库');
    expect(first).toContain('データベース設計 스키마');
    expect(first).not.toContain('unrelated');
    expect(second).not.toContain('data-blocked-rows');
  });
});
