import { apiGet } from './client';

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

/**
 * A session PR (`ApiPr`): `GET /api/sessions/{id}/prs` and the session
 * stream's `prs.changed` frames.
 */
export interface ApiPr {
  number: number;
  title: string;
  state: string;
  backend: string;
  externalNumber: number | null;
  issueNumber: number | null;
  autoMerge: boolean;
  updatedAt: string;
  checks: ApiPrChecks | null;
  /** Why the daemon's last read of the checks failed. */
  checksError: string | null;
}

export function getSessionPrs(sessionId: string): Promise<ApiPr[]> {
  return apiGet<ApiPr[]>(`/sessions/${encodeURIComponent(sessionId)}/prs`);
}
