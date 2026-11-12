import { describe, expect, test } from 'bun:test';
import { githubBinding, groupByRepo, parseRepo, repoLabel } from './repo';

describe('repo bindings', () => {
  test('a GitHub binding is shown as owner/repo and carries no path', () => {
    expect(parseRepo(githubBinding('acme/app'))).toEqual({ kind: 'github', fullName: 'acme/app' });
    expect(repoLabel('github:acme/app')).toBe('acme/app');
  });

  test('a local binding is shown by the checkout path, not the .git directory', () => {
    expect(parseRepo('local:/work/app/.git')).toEqual({ kind: 'local', path: '/work/app' });
    expect(repoLabel('local:/work/app/.git')).toBe('/work/app');
    expect(repoLabel('local:/work/bare')).toBe('/work/bare');
  });
});

describe('grouping by repository', () => {
  const sessions = [
    { id: 'a', repo: 'github:zed/app' },
    { id: 'b', repo: null },
    { id: 'c', repo: 'local:/work/app/.git' },
    { id: 'd', repo: 'github:zed/app' },
    { id: 'e' },
  ];

  test('repo-less sessions stay in the flat bucket, in order', () => {
    expect(groupByRepo(sessions).flat.map((s) => s.id)).toEqual(['b', 'e']);
  });

  test('each repository gets one group, sorted by its label', () => {
    const { groups } = groupByRepo(sessions);
    expect(groups.map(([repo, items]) => [repo, items.map((s) => s.id)])).toEqual([
      ['local:/work/app/.git', ['c']],
      ['github:zed/app', ['a', 'd']],
    ]);
  });

  test('a list with no bound session has no groups', () => {
    expect(groupByRepo([{ id: 'x' }]).groups).toEqual([]);
  });
});
