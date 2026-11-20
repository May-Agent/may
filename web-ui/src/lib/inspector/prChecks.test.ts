import { describe, expect, test } from 'bun:test';
import type { ApiPr, ApiPrCheck } from '../api/prs';
import {
  checkDurationMs,
  checkProgress,
  hasPrDetail,
  livePrs,
  prFailureDetail,
  prOpenedProvenance,
  prRollup,
} from './prChecks';

const check = (state: ApiPrCheck['state'], startedAt: string | null, finishedAt: string | null): ApiPrCheck => ({
  name: '测试 テスト 테스트',
  state,
  startedAt,
  finishedAt,
  url: null,
});

const pr = (state: string, pending: number, passed: number, failed: number): ApiPr => ({
  id: 1,
  repoKey: '',
  sessionId: 'sess-1',
  orphaned: false,
  requireReview: false,
  number: 1,
  title: 'wire it',
  state,
  backend: 'github',
  issueNumber: null,
  autoMerge: false,
  baseSha: 'abc1234',
  headSha: 'def5678',
  mergeCommitSha: null,
  createdAt: '2026-11-15T10:00:00Z',
  updatedAt: '',
  mergedAt: null,
  checks: { pending, passed, failed, items: [] },
  checksError: null,
  events: [],
});

describe('PR checks', () => {
  test('the rollup is the traffic light, merging overriding it', () => {
    expect(prRollup(pr('open', 1, 0, 1))).toBe('failed');
    expect(prRollup(pr('open', 1, 3, 0))).toBe('running');
    expect(prRollup(pr('open', 0, 4, 0))).toBe('passed');
    expect(prRollup(pr('merging', 2, 0, 0))).toBe('merging');
    expect(prRollup({ ...pr('open', 0, 0, 0), checks: null })).toBe('unknown');
    expect(prRollup(pr('rejected', 0, 0, 0))).toBe('failed');
    expect(prRollup(pr('merged', 0, 0, 0))).toBe('passed');
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

  test('only in-flight PRs are listed by livePrs', () => {
    const all = [pr('open', 0, 1, 0), pr('merged', 0, 1, 0), pr('closed', 0, 0, 0)];
    expect(livePrs(all)).toHaveLength(1);
  });

  test('prFailureDetail and prOpenedProvenance surface detail and opening provenance', () => {
    const rejected: ApiPr = {
      ...pr('rejected', 0, 0, 0),
      repoKey: 'https://github.com/org/repo.git',
      events: [
        {
          sessionId: 'sess-1',
          actor: 'main',
          messageId: 7,
          kind: 'opened',
          detail: null,
          createdAt: '2026-11-15T10:00:00Z',
        },
        {
          sessionId: 'sess-1',
          actor: 'main',
          messageId: 8,
          kind: 'rejected',
          detail: 'needs rebase',
          createdAt: '2026-11-15T10:02:00Z',
        },
      ],
    };
    expect(prFailureDetail(rejected)).toBe('needs rebase');
    expect(prOpenedProvenance(rejected)?.actor).toBe('main');
    expect(hasPrDetail(rejected)).toBe(true);
    expect(hasPrDetail({ ...pr('open', 0, 0, 0), checks: null })).toBe(false);
  });
});
