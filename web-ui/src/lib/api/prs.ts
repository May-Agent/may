import { apiGet, apiPost } from './client';

/** One check of a PR's head commit (`ApiPrCheck`). */
export interface ApiPrCheck {
  name: string;
  state: 'queued' | 'running' | 'passed' | 'failed';
  /** RFC 3339; absent while queued. */
  startedAt: string | null;
  /** RFC 3339; absent until the check concludes. */
  finishedAt: string | null;
  /** Where GitHub shows the check, with its detail and log. */
  url: string | null;
}

/** The checks rollup of a PR and each check (`ApiPrChecks`). */
export interface ApiPrChecks {
  pending: number;
  passed: number;
  failed: number;
  items: ApiPrCheck[];
}

/** One transition event recorded on a PR (`ApiPrEvent`). */
export interface ApiPrEvent {
  sessionId: string;
  actor: string;
  messageId: number | null;
  kind: string;
  detail: string | null;
  createdAt: string;
}

/**
 * A PR over the wire (`ApiPr`): `GET /api/prs`, `GET /api/sessions/{id}/prs`,
 * and live event stream updates.
 */
export interface ApiPr {
  id: number;
  repoKey: string;
  sessionId: string;
  orphaned: boolean;
  requireReview: boolean;
  number: number;
  title: string;
  state: string;
  backend: string;
  issueNumber: number | null;
  autoMerge: boolean;
  baseSha: string;
  headSha: string | null;
  mergeCommitSha: string | null;
  createdAt: string;
  updatedAt: string;
  mergedAt: string | null;
  checks: ApiPrChecks | null;
  /** Why the daemon's last read of the checks failed. */
  checksError: string | null;
  events: ApiPrEvent[];
}

export interface ListPrsParams {
  repo?: string;
  sessionId?: string;
  orphaned?: boolean;
  state?: string;
  forSession?: string;
}

export function listPrs(params: ListPrsParams = {}): Promise<ApiPr[]> {
  const qs = new URLSearchParams();
  if (params.repo) qs.set('repo', params.repo);
  if (params.sessionId) qs.set('sessionId', params.sessionId);
  if (params.orphaned !== undefined) qs.set('orphaned', String(params.orphaned));
  if (params.state) qs.set('state', params.state);
  if (params.forSession) qs.set('forSession', params.forSession);
  const query = qs.toString();
  return apiGet<ApiPr[]>(`/prs${query ? `?${query}` : ''}`);
}

export function getSessionPrs(sessionId: string): Promise<ApiPr[]> {
  return apiGet<ApiPr[]>(`/sessions/${encodeURIComponent(sessionId)}/prs`);
}

export function mergePr(prId: number): Promise<ApiPr> {
  return apiPost<ApiPr>(`/prs/${prId}/merge`, {});
}

export function closePr(prId: number): Promise<ApiPr> {
  return apiPost<ApiPr>(`/prs/${prId}/close`, {});
}
