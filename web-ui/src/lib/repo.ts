import type { GithubRepo } from './api/githubRepos';

/** What a session's repo binding names. A forge project has no path on this
 * machine's behalf; a local one is shown by the checkout's path. */
export type RepoRef =
  | { kind: 'github'; fullName: string }
  | { kind: 'gitlab'; host: string; path: string }
  | { kind: 'local'; path: string };

/** Parse a stored binding: `github:owner/repo`, `gitlab:<host>/<path>` or
 * `local:<git common dir>`. */
export function parseRepo(repo: string): RepoRef {
  if (repo.startsWith('github:')) {
    return { kind: 'github', fullName: repo.slice('github:'.length) };
  }
  if (repo.startsWith('gitlab:')) {
    const rest = repo.slice('gitlab:'.length);
    const slash = rest.indexOf('/');
    if (slash > 0) {
      return { kind: 'gitlab', host: rest.slice(0, slash), path: rest.slice(slash + 1) };
    }
    return { kind: 'gitlab', host: '', path: rest };
  }
  const identity = repo.startsWith('local:') ? repo.slice('local:'.length) : repo;
  // The identity of a checkout is its `.git` directory.
  const path = identity.endsWith('/.git') ? identity.slice(0, -'/.git'.length) : identity;
  return { kind: 'local', path };
}

/** Display label for a repo binding: `owner/repo` for a GitHub project,
 * `<host>/<path>` for a GitLab one, the checkout's path for a local one. */
export function repoLabel(repo: string): string {
  const ref = parseRepo(repo);
  switch (ref.kind) {
    case 'github':
      return ref.fullName;
    case 'gitlab':
      return ref.host ? `${ref.host}/${ref.path}` : ref.path;
    default:
      return ref.path;
  }
}

/** The session binding for a GitHub `owner/repo`. */
export function githubBinding(fullName: string): string {
  return `github:${fullName}`;
}

/** The binding a picker entry names: its `binding` field when the daemon
 * sends one, the `github:` shorthand of `fullName` otherwise. */
export function repoBinding(repo: GithubRepo): string {
  return repo.binding ?? githubBinding(repo.fullName);
}

/** The forge an entry belongs to (`github` when the field is absent). */
export function repoForge(repo: GithubRepo): string {
  return repo.forge ?? (repo.binding?.startsWith('gitlab:') ? 'gitlab' : 'github');
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

/** Repositories whose `fullName` contains every whitespace-separated term
 * of `query`, case-insensitively; the list order is kept. */
export function filterRepos(repos: GithubRepo[], query: string): GithubRepo[] {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return repos;
  return repos.filter((repo) => {
    const name = repo.fullName.toLowerCase();
    return terms.every((term) => name.includes(term));
  });
}
