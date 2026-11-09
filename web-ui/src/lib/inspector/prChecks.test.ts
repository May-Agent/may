import { describe, expect, test } from 'bun:test';
import type { ApiPr, ApiPrCheck } from '../api/prs';
import { checkDurationMs, checkProgress, livePrs, prRollup } from './prChecks';

const check = (state: ApiPrCheck['state'], startedAt: string | null, finishedAt: string | null): ApiPrCheck => ({
  name: '测试 テスト 테스트',
  state,
  startedAt,
  finishedAt,
  url: null,
});

const pr = (state: string, pending: number, passed: number, failed: number): ApiPr => ({
  number: 1,
  title: 'wire it',
  state,
  backend: 'github',
  externalNumber: 1,
  issueNumber: null,
  autoMerge: false,
  updatedAt: '',
  checks: { pending, passed, failed, items: [] },
  checksError: null,
});

describe('PR checks', () => {
  test('the rollup is the traffic light, merging overriding it', () => {
    expect(prRollup(pr('open', 1, 0, 1))).toBe('failed');
    expect(prRollup(pr('open', 1, 3, 0))).toBe('running');
    expect(prRollup(pr('open', 0, 4, 0))).toBe('passed');
    expect(prRollup(pr('merging', 2, 0, 0))).toBe('merging');
    expect(prRollup({ ...pr('open', 0, 0, 0), checks: null })).toBe('unknown');
  });

  test('progress counts concluded checks over all of them', () => {
    expect(checkProgress({ pending: 2, passed: 1, failed: 1, items: [] })).toEqual({
      finished: 2,
      total: 4,
    });
  });

  test('a running check measures to now, a concluded one to its end', () => {
    const now = Date.parse('2026-11-09T10:03:00Z');
    expect(checkDurationMs(check('running', '2026-11-09T10:00:00Z', null), now)).toBe(180_000);
    expect(
      checkDurationMs(check('failed', '2026-11-09T10:00:00Z', '2026-11-09T10:02:05Z'), now),
    ).toBe(125_000);
    expect(checkDurationMs(check('queued', null, null), now)).toBeNull();
  });

  test('only in-flight PRs are listed', () => {
    const all = [pr('open', 0, 1, 0), pr('merged', 0, 1, 0), pr('closed', 0, 0, 0)];
    expect(livePrs(all)).toHaveLength(1);
  });
});
