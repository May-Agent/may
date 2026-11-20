import { apiGet } from './client';

export interface GithubRepo {
  /** `owner/repo` on github.com, `<host>/<path>` elsewhere. */
  fullName: string;
  /** A local checkout of it was used recently. */
  recent: boolean;
  /** The canonical session binding (`github:owner/repo`, `gitlab:<host>/<path>`). */
  binding?: string;
  /** The forge kind: `github` or `gitlab`. */
  forge?: string;
  /** The forge host (`github.com`, `gitlab.com`, a self-hosted name). */
  host?: string;
}

export interface GithubReposResponse {
  repos: GithubRepo[];
}

/** Repositories a new chat can be bound to, recently used ones first. */
export function listGithubRepos(): Promise<GithubReposResponse> {
  return apiGet<GithubReposResponse>('/github/repos');
}
