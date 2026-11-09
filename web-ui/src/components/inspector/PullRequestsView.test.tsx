import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ApiPr } from '@/lib/api/prs';
import { PullRequestsView } from './PullRequestsView';

const pr: ApiPr = {
  number: 12,
  title: '修复图片渲染 画像 이미지',
  state: 'open',
  backend: 'github',
  externalNumber: 12,
  issueNumber: null,
  autoMerge: false,
  updatedAt: '',
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
});
