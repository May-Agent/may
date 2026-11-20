import { describe, expect, test } from 'bun:test';
import { isValidElement, type ReactElement, type ReactNode } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { GithubRepo } from '@/lib/api/githubRepos';
import { filterRepos } from '@/lib/repo';
import { RepoPickerList, type RepoListState, type RepoPickerLabels } from './RepoPickerList';

const REPOS: GithubRepo[] = [
  { fullName: 'May-Agent/may', recent: true, binding: 'github:May-Agent/may', forge: 'github', host: 'github.com' },
  { fullName: 'lexoliu/aither', recent: false, binding: 'github:lexoliu/aither', forge: 'github', host: 'github.com' },
  { fullName: 'lexoliu/waterui', recent: false, binding: 'github:lexoliu/waterui', forge: 'github', host: 'github.com' },
  {
    fullName: 'gitlab.com/研究/プロジェクト/한국어',
    recent: false,
    binding: 'gitlab:gitlab.com/研究/プロジェクト/한국어',
    forge: 'gitlab',
    host: 'gitlab.com',
  },
];

const LABELS: RepoPickerLabels = {
  search: 'Search',
  loading: 'Loading',
  empty: 'Nothing',
  recent: 'Recent',
  all: 'All',
};

/** Every `onSelect` handler in the element tree, in document order. */
function selectHandlers(node: ReactNode, out: Array<() => void> = []): Array<() => void> {
  if (Array.isArray(node)) {
    for (const child of node) selectHandlers(child, out);
  } else if (isValidElement(node)) {
    const props = (node as ReactElement<{ onSelect?: () => void; children?: ReactNode }>).props;
    if (props.onSelect) out.push(props.onSelect);
    selectHandlers(props.children, out);
  }
  return out;
}

describe('filterRepos', () => {
  test('matches every term case-insensitively and keeps the order', () => {
    expect(filterRepos(REPOS, 'LEXOLIU')).toEqual([REPOS[1], REPOS[2]]);
    expect(filterRepos(REPOS, 'lexoliu water')).toEqual([REPOS[2]]);
    expect(filterRepos(REPOS, '  ')).toEqual(REPOS);
    expect(filterRepos(REPOS, 'nope')).toEqual([]);
  });

  test('matches CJK and nested GitLab subgroup paths', () => {
    expect(filterRepos(REPOS, '研究')).toEqual([REPOS[3]]);
    expect(filterRepos(REPOS, 'gitlab.com プロジェクト')).toEqual([REPOS[3]]);
    expect(filterRepos(REPOS, '한국어')).toEqual([REPOS[3]]);
  });
});

describe('RepoPickerList', () => {
  const ready: RepoListState = { status: 'ready', repos: REPOS };

  test('shows only the entries the search text matches', () => {
    const markup = renderToStaticMarkup(
      <RepoPickerList
        state={ready}
        query="aither"
        onQueryChange={() => {}}
        onPick={() => {}}
        labels={LABELS}
      />,
    );
    expect(markup).toContain('lexoliu/aither');
    expect(markup).not.toContain('lexoliu/waterui');
    expect(markup).not.toContain('May-Agent/may');
  });

  test('lists recently used repositories under their own heading first', () => {
    const markup = renderToStaticMarkup(
      <RepoPickerList
        state={ready}
        query=""
        onQueryChange={() => {}}
        onPick={() => {}}
        labels={LABELS}
      />,
    );
    expect(markup.indexOf('Recent')).toBeLessThan(markup.indexOf('May-Agent/may'));
    expect(markup.indexOf('May-Agent/may')).toBeLessThan(markup.indexOf('All'));
  });

  test('starting from a picked entry binds the session to that repository', () => {
    const started: string[] = [];
    const element = RepoPickerList({
      state: ready,
      query: 'waterui',
      onQueryChange: () => {},
      onPick: (repo) => started.push(repo),
      labels: LABELS,
    });
    const handlers = selectHandlers(element);
    expect(handlers).toHaveLength(1);
    handlers[0]();
    expect(started).toEqual(['github:lexoliu/waterui']);
  });

  test('a GitLab entry binds its canonical gitlab: binding', () => {
    const started: string[] = [];
    const element = RepoPickerList({
      state: ready,
      query: '研究',
      onQueryChange: () => {},
      onPick: (repo) => started.push(repo),
      labels: LABELS,
    });
    const handlers = selectHandlers(element);
    expect(handlers).toHaveLength(1);
    handlers[0]();
    expect(started).toEqual(['gitlab:gitlab.com/研究/プロジェクト/한국어']);
  });

  test('offers no entry while loading or unauthenticated', () => {
    const loading = RepoPickerList({
      state: { status: 'loading' },
      query: '',
      onQueryChange: () => {},
      onPick: () => {},
      labels: LABELS,
    });
    expect(selectHandlers(loading)).toHaveLength(0);
    expect(renderToStaticMarkup(loading)).toContain('Loading');

    const unauthenticated = RepoPickerList({
      state: { status: 'unauthenticated', message: 'Sign in to GitHub' },
      query: '',
      onQueryChange: () => {},
      onPick: () => {},
      labels: LABELS,
    });
    expect(selectHandlers(unauthenticated)).toHaveLength(0);
    expect(renderToStaticMarkup(unauthenticated)).toContain('Sign in to GitHub');
  });
});
