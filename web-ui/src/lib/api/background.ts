import { apiDelete, apiGet, apiPost } from './client';

/** A background terminal job's state (`WireJobStatus`, tagged by `state`). */
export type BackgroundJobStatus =
  | { state: 'running' }
  | { state: 'completed'; exit_code: number }
  | { state: 'failed'; error: string }
  | { state: 'killed' };

/** One background terminal job of a session (`WireJob`). */
export interface BackgroundJob {
  pid: number;
  taskId: string;
  executionKey?: string;
  script: string;
  mode: string;
  elapsedMs: number;
  status: BackgroundJobStatus;
  outputPath?: string;
}

/** One subagent run in flight (`WireSubagentRun`). */
export interface SubagentRun {
  runId: number;
  agent: string;
  prompt: string;
  elapsedMs: number;
  promptTokens: number;
  completionTokens: number;
  requests: number;
}

/**
 * A session's background work: `GET /api/sessions/{id}/background` and the
 * `session_background` event of `/api/events`.
 */
export interface SessionBackground {
  sessionId: string;
  jobs: BackgroundJob[];
  subagents: SubagentRun[];
}

const base = (sessionId: string) => `/sessions/${encodeURIComponent(sessionId)}`;

export function getSessionBackground(sessionId: string): Promise<SessionBackground> {
  return apiGet<SessionBackground>(`${base(sessionId)}/background`);
}

/** Kill a running job; `ok` is false when it was no longer running. */
export function killBackgroundJob(sessionId: string, pid: number): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>(`${base(sessionId)}/jobs/${pid}/kill`);
}

/** Stop one subagent run. */
export function cancelSubagentRun(sessionId: string, runId: number): Promise<{ stopped: number }> {
  return apiDelete<{ stopped: number }>(`${base(sessionId)}/subagents/${runId}`);
}
