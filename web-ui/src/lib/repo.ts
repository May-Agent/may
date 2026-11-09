import type { GithubRepo } from './api/githubRepos';

/** What a session's repo binding names. A GitHub project has no path on this
 * machine's behalf; a local one is shown by the checkout's path. */
export type RepoRef =
  | { kind: 'github'; fullName: string }
  | { kind: 'local'; path: string };

/** Parse a stored binding: `github:owner/repo` or `local:<git common dir>`. */
export function parseRepo(repo: string): RepoRef {
  if (repo.startsWith('github:')) {
    return { kind: 'github', fullName: repo.slice('github:'.length) };
  }
  const identity = repo.startsWith('local:') ? repo.slice('local:'.length) : repo;
  // The identity of a checkout is its `.git` directory.
  const path = identity.endsWith('/.git') ? identity.slice(0, -'/.git'.length) : identity;
  return { kind: 'local', path };
}

/** Display label for a repo binding: `owner/repo` for a GitHub project (never
 * a path), the checkout's path for a local one. */
export function repoLabel(repo: string): string {
  const ref = parseRepo(repo);
  return ref.kind === 'github' ? ref.fullName : ref.path;
}

/** The session binding for a GitHub `owner/repo`. */
export function githubBinding(fullName: string): string {
  return `github:${fullName}`;
}

/** A session list entry that may be bound to a repository. */
interface Bindable {
  id: string;
  repo?: string | null;
}

/** Sessions split into the repo-less bucket, which stays flat and ungrouped,
 * and one group per repository, sorted by label. Order within each is kept. */
export function groupByRepo<T extends Bindable>(
  items: T[],
): { flat: T[]; groups: [string, T[]][] } {
  const flat: T[] = [];
  const byRepo = new Map<string, T[]>();
  for (const item of items) {
    if (item.repo) {
      const list = byRepo.get(item.repo) ?? [];
      list.push(item);
      byRepo.set(item.repo, list);
    } else {
      flat.push(item);
    }
  }
  const groups = [...byRepo.entries()].sort(([a], [b]) =>
    repoLabel(a).localeCompare(repoLabel(b)),
  );
  return { flat, groups };
}

/** Repositories whose `owner/repo` contains every whitespace-separated term
 * of `query`, case-insensitively; the list order is kept. */
export function filterRepos(repos: GithubRepo[], query: string): GithubRepo[] {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return repos;
  return repos.filter((repo) => {
    const name = repo.fullName.toLowerCase();
    return terms.every((term) => name.includes(term));
  });
}
