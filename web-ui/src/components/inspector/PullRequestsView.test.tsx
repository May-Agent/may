import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ApiPr } from '@/lib/api/prs';
import { PullRequestsView } from './PullRequestsView';

const pr: ApiPr = {
  id: 1,
  repoKey: '',
  sessionId: 'sess-1',
  orphaned: false,
  requireReview: false,
  number: 12,
  title: '修复图片渲染 画像 이미지',
  state: 'open',
  backend: 'github',
  externalNumber: 12,
  issueNumber: null,
  autoMerge: false,
  baseSha: 'abc1234',
  headSha: 'def5678',
  mergeCommitSha: null,
  createdAt: '2026-11-15T10:00:00Z',
  updatedAt: '',
  mergedAt: null,
  checks: {
    pending: 2,
    passed: 1,
    failed: 1,
    items: [
      { name: 'fmt', state: 'passed', startedAt: null, finishedAt: null, url: null },
      { name: 'test', state: 'failed', startedAt: null, finishedAt: null, url: 'https://x/1' },
    ],
  },
  checksError: null,
  events: [],
};

describe('PullRequestsView', () => {
  test('a collapsed row shows the title, the finished-of-total figure and the rollup', () => {
    const markup = renderToStaticMarkup(<PullRequestsView prs={[pr]} />);
    expect(markup).toContain('修复图片渲染 画像 이미지');
    expect(markup).toContain('2/4');
    expect(markup).toContain('aria-expanded="false"');
    // The checks stay folded until the row opens.
    expect(markup).not.toContain('>fmt<');
  });

  test('a PR whose checks were not read has nothing to expand', () => {
    const markup = renderToStaticMarkup(
      <PullRequestsView prs={[{ ...pr, checks: null }]} />,
    );
    expect(markup).not.toContain('aria-expanded');
  });

  test('orphaned or review-required PRs show failure detail and Merge/Close actions', () => {
    const orphanedRejected: ApiPr = {
      ...pr,
      orphaned: true,
      requireReview: true,
      state: 'rejected',
      repoKey: 'https://github.com/org/repo.git',
      events: [
        {
          sessionId: 'sess-1',
          actor: 'agent-1',
          messageId: 42,
          kind: 'rejected',
          detail: 'merge conflict on src/lib.rs',
          createdAt: '2026-11-15T10:05:00Z',
        },
      ],
    };
    const markup = renderToStaticMarkup(
      <PullRequestsView
        prs={[orphanedRejected]}
        onMerge={async () => {}}
        onClose={async () => {}}
      />,
    );
    expect(markup).toContain('merge conflict on src/lib.rs');
    expect(markup).toContain('Orphaned');
    expect(markup).toContain('Rejected');
    expect(markup).toContain('Merge PR');
    expect(markup).toContain('Close PR');
  });

  test('terminal PRs do not render Merge/Close action buttons', () => {
    const merged: ApiPr = {
      ...pr,
      orphaned: true,
      state: 'merged',
      mergeCommitSha: '9999aaa',
    };
    const markup = renderToStaticMarkup(
      <PullRequestsView prs={[merged]} onMerge={async () => {}} onClose={async () => {}} />,
    );
    expect(markup).not.toContain('Merge PR');
    expect(markup).not.toContain('Close PR');
  });
});
