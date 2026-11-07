import type { GithubRepo } from './api/githubRepos';

/** Display label for a repo binding: `owner/repo` for github: specs, the
 * path basename for local: keys. */
export function repoLabel(repo: string): string {
  if (repo.startsWith('github:')) {
    return repo.slice(7);
  }
  const rest = repo.startsWith('local:') ? repo.slice(6) : repo;
  return rest.split('/').filter(Boolean).pop() ?? rest;
}

/** The session binding for a GitHub `owner/repo`. */
export function githubBinding(fullName: string): string {
  return `github:${fullName}`;
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
