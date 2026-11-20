import { describe, expect, test } from 'bun:test';
import { githubBinding, groupByRepo, parseRepo, repoBinding, repoForge, repoLabel } from './repo';

describe('repo bindings', () => {
  test('a GitHub binding is shown as owner/repo and carries no path', () => {
    expect(parseRepo(githubBinding('acme/app'))).toEqual({ kind: 'github', fullName: 'acme/app' });
    expect(repoLabel('github:acme/app')).toBe('acme/app');
  });

  test('a GitLab binding carries its host and nested path', () => {
    expect(parseRepo('gitlab:gitlab.com/group/sub/repo')).toEqual({
      kind: 'gitlab',
      host: 'gitlab.com',
      path: 'group/sub/repo',
    });
    expect(repoLabel('gitlab:gitlab.com/group/sub/repo')).toBe('gitlab.com/group/sub/repo');
    expect(parseRepo('gitlab:git.corp.example/team/app')).toEqual({
      kind: 'gitlab',
      host: 'git.corp.example',
      path: 'team/app',
    });
    expect(repoLabel('gitlab:git.corp.example/team/app')).toBe('git.corp.example/team/app');
  });

  test('a local binding is shown by the checkout path, not the .git directory', () => {
    expect(parseRepo('local:/work/app/.git')).toEqual({ kind: 'local', path: '/work/app' });
    expect(repoLabel('local:/work/app/.git')).toBe('/work/app');
    expect(repoLabel('local:/work/bare')).toBe('/work/bare');
  });

  test('picker entries bind through their canonical binding', () => {
    expect(
      repoBinding({ fullName: 'acme/app', recent: false, binding: 'github:acme/app' }),
    ).toBe('github:acme/app');
    expect(
      repoBinding({
        fullName: 'gitlab.com/group/sub/repo',
        recent: false,
        binding: 'gitlab:gitlab.com/group/sub/repo',
        forge: 'gitlab',
        host: 'gitlab.com',
      }),
    ).toBe('gitlab:gitlab.com/group/sub/repo');
    // A daemon that sends no new fields still binds a GitHub shorthand.
    expect(repoBinding({ fullName: 'acme/app', recent: false })).toBe('github:acme/app');
    expect(repoForge({ fullName: 'acme/app', recent: false })).toBe('github');
    expect(repoForge({ fullName: 'x', recent: false, forge: 'gitlab' })).toBe('gitlab');
  });
});

describe('grouping by repository', () => {
  const sessions = [
    { id: 'a', repo: 'github:zed/app' },
    { id: 'b', repo: null },
    { id: 'c', repo: 'local:/work/app/.git' },
    { id: 'd', repo: 'github:zed/app' },
    { id: 'e' },
    { id: 'f', repo: 'gitlab:gitlab.com/group/sub/repo' },
  ];

  test('repo-less sessions stay in the flat bucket, in order', () => {
    expect(groupByRepo(sessions).flat.map((s) => s.id)).toEqual(['b', 'e']);
  });

  test('each repository gets one group, sorted by its label', () => {
    const { groups } = groupByRepo(sessions);
    expect(groups.map(([repo, items]) => [repo, items.map((s) => s.id)])).toEqual([
      ['local:/work/app/.git', ['c']],
      ['gitlab:gitlab.com/group/sub/repo', ['f']],
      ['github:zed/app', ['a', 'd']],
    ]);
  });

  test('a list with no bound session has no groups', () => {
    expect(groupByRepo([{ id: 'x' }]).groups).toEqual([]);
  });
});
