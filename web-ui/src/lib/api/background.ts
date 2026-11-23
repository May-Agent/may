import { apiDelete, apiGet, apiPost } from './client';

/** A background terminal job's state (`WireJobStatus`, tagged by `state`). */
export type BackgroundJobStatus =
  | { state: 'running' }
  | { state: 'completed'; exit_code: number }
  | { state: 'failed'; error: string }
  | { state: 'killed'; cause: { kind: 'user' } | { kind: 'timeout'; timeoutMs: number; started: string; killed: string } | { kind: 'memory_runaway'; footprint_bytes: number; capacity_bytes: number; domain: 'machine' | 'container' } };

/** One background terminal job of a session (`WireJob`). */
export interface BackgroundJob {
  pid: number;
  executionKey?: string;
  script: string;
  /** The interpreter the script runs under (`"shell"` or `"python"`). */
  interpreter: string;
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

/** Execution state of one pipeline stage (`WireStageStatus`). */
export type PipelineStageStatus =
  | { state: 'pending' }
  | { state: 'running' }
  | { state: 'exited'; code: number };

/** Metadata for one `0`-indexed stage of a pipeline (`WirePipelineStage`). */
export interface PipelineStage {
  step: number;
  command: string;
  status: PipelineStageStatus;
  ownDurationMs?: number;
  retained: boolean;
  uiBytes: number;
  totalLines: number;
}

/** A running or retained multi-stage shell pipeline (`WirePipeline`). */
export interface BackgroundPipeline {
  pid: number;
  launchId: number;
  script?: string;
  jobStatus: BackgroundJobStatus;
  elapsedMs: number;
  expiresAt?: string;
  activeStep?: number | null;
  stages: PipelineStage[];
}

/** Incremental terminal or pipeline-stage delta (`WireTerminalDelta`). */
export interface TerminalDelta {
  cursor: number;
  /** Base64-encoded UTF-8/ANSI bytes since the previous cursor. */
  bytes: string;
  status: BackgroundJobStatus;
  totalBytes?: number;
  stageStatus?: PipelineStageStatus;
}

/**
 * A session's background work: `GET /api/sessions/{id}/background` and the
 * stream's `job.*`/`subagent.*`/`pipeline.*` deltas on `/api/sessions/{id}/events`.
 */
export interface SessionBackground {
  sessionId: string;
  jobs: BackgroundJob[];
  subagents: SubagentRun[];
  pipelines?: BackgroundPipeline[];
}

const base = (sessionId: string) => `/sessions/${encodeURIComponent(sessionId)}`;

export function getSessionBackground(sessionId: string): Promise<SessionBackground> {
  return apiGet<SessionBackground>(`${base(sessionId)}/background`);
}

/** Read colored UI output bytes of pipeline stage `step` since `cursor`. */
export function readPipelineStage(
  sessionId: string,
  pid: number,
  step: number,
  cursor: number,
  launchId?: number,
): Promise<TerminalDelta> {
  const params = new URLSearchParams({
    pid: String(pid),
    step: String(step),
    cursor: String(cursor),
  });
  if (launchId !== undefined) params.set('launchId', String(launchId));
  return apiGet<TerminalDelta>(`${base(sessionId)}/jobs/read?${params.toString()}`);
}

const utf8Decoder = new TextDecoder();

/** Decode a base64 `WireTerminalDelta.bytes` string into UTF-8 text. */
export function decodeBase64Utf8(encoded: string): string {
  if (!encoded) return '';
  const bin = atob(encoded);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) {
    bytes[i] = bin.charCodeAt(i);
  }
  return utf8Decoder.decode(bytes);
}

/** Kill a running job; `ok` is false when it was no longer running. */
export function killBackgroundJob(sessionId: string, pid: number): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>(`${base(sessionId)}/jobs/${pid}/kill`);
}

/** Stop one subagent run. */
export function cancelSubagentRun(sessionId: string, runId: number): Promise<{ stopped: number }> {
  return apiDelete<{ stopped: number }>(`${base(sessionId)}/subagents/${runId}`);
}
