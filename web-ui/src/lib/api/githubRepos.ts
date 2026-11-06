import { apiGet } from './client';

export interface GithubRepo {
  /** `owner/repo`. */
  fullName: string;
  /** A local checkout of it was used recently. */
  recent: boolean;
}

export interface GithubReposResponse {
  repos: GithubRepo[];
}

/** Repositories a new chat can be bound to, recently used ones first. */
export function listGithubRepos(): Promise<GithubReposResponse> {
  return apiGet<GithubReposResponse>('/github/repos');
}
