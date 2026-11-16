import { describe, expect, test } from 'bun:test';
import { openCount, openGroups, referenceLabel, rootReferences } from './rows';
import type { TaskRow } from '../types';

const row = (section: TaskRow['section'], title: string, extra: Partial<TaskRow> = {}): TaskRow => ({
  section,
  title,
  detail: '',
  blocking: null,
  references: [],
  ...extra,
});

describe('task rows', () => {
  const rows = [
    row('roots', 'May-Agent/may#746', {
      references: [{ repository: 'May-Agent/may', number: 746, pullRequest: false }],
    }),
    row('queued', 'ship it'),
    row('doing', 'write report', { blocking: 'q1' }),
  ];

  test('open work is grouped Doing before Queued, without roots', () => {
    expect(openGroups(rows).map((g) => [g.section, g.rows.map((r) => r.title)])).toEqual([
      ['doing', ['write report']],
      ['queued', ['ship it']],
    ]);
    expect(openCount(rows)).toBe(2);
  });

  test('no open rows leave no groups', () => {
    expect(openGroups([rows[0]])).toEqual([]);
    expect(openCount([])).toBe(0);
  });

  test('roots are listed as references', () => {
    expect(rootReferences(rows).map(referenceLabel)).toEqual(['May-Agent/may#746']);
    expect(referenceLabel({ repository: null, number: 3, pullRequest: true })).toBe('PR #3');
  });
});
