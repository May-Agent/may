import type { BackgroundJob, SessionBackground } from '../api/background';

/** Jobs and subagent runs the user asked to stop, awaiting confirmation. */
export interface StopRequests {
  jobs: number[];
  subagents: number[];
}

export const NO_STOP_REQUESTS: StopRequests = { jobs: [], subagents: [] };

export function isJobRunning(job: BackgroundJob): boolean {
  return job.status.state === 'running';
}

/**
 * Drop stop requests the snapshot settles: a job that no longer runs, a
 * subagent run that is gone. What still runs keeps its request so the
 * button keeps showing progress.
 */
export function settleStopRequests(
  requests: StopRequests,
  snapshot: SessionBackground,
): StopRequests {
  const running = new Set(snapshot.jobs.filter(isJobRunning).map((j) => j.pid));
  const live = new Set(snapshot.subagents.map((s) => s.runId));
  const jobs = requests.jobs.filter((pid) => running.has(pid));
  const subagents = requests.subagents.filter((id) => live.has(id));
  if (jobs.length === requests.jobs.length && subagents.length === requests.subagents.length) {
    return requests;
  }
  return { jobs, subagents };
}

/** Running jobs first, newest first within each half. */
export function orderJobs(jobs: BackgroundJob[]): BackgroundJob[] {
  return [...jobs].sort((a, b) => {
    const ra = isJobRunning(a) ? 0 : 1;
    const rb = isJobRunning(b) ? 0 : 1;
    if (ra !== rb) return ra - rb;
    return a.elapsedMs - b.elapsedMs;
  });
}

/** Compact duration: `45s`, `3m 12s`, `1h 04m`. */
export function formatElapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, '0')}m`;
  if (m > 0) return `${m}m ${String(s).padStart(2, '0')}s`;
  return `${s}s`;
}

/** Compact token count: `950`, `12.3k`, `1.2M`. */
export function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/** First line of a job's script, the part a list row shows. */
export function scriptHeadline(script: string): string {
  const line = script.split('\n').find((l) => l.trim().length > 0) ?? '';
  return line.trim();
}
