import { ApiError, apiSseUrl } from './client';
import { parseNamedSseStream } from '../stream/sse';
import { markAuthRequired } from '../stores/authGate';
import type { ChatStreamEvent } from '../types';
import type { SessionBackground } from './background';

const RECONNECT_DELAY_MS = 1000;

/** One `session_agent` frame; its `start` body carries the prompt's message id. */
export interface SessionAgentFrame {
  sessionId: string;
  turnId: string;
  at: string;
  body: {
    kind: 'start' | 'agent' | 'withdrawn';
    messageId?: number | null;
    [key: string]: unknown;
  };
}

/** One `session_turn` frame: a turn lifecycle event. */
export interface SessionTurnFrame {
  sessionId: string;
  event: ChatStreamEvent['kind'];
  data: Record<string, unknown>;
}

/** The raw event one session-stream watcher sees. */
export type SessionEventHandler = (
  kind: string,
  data: Record<string, unknown>,
  id: string | null,
) => void;

/** The background snapshot a watcher's `job.*`/`subagent.*` deltas apply to. */
export interface BackgroundState {
  jobs: SessionBackground['jobs'];
  subagents: SessionBackground['subagents'];
}

/** Apply one `job.*`/`subagent.*` frame to `state`; whether it handled `kind`. */
export function applyBackgroundDelta(
  state: BackgroundState,
  kind: string,
  data: Record<string, unknown>,
): boolean {
  switch (kind) {
    case 'job.upserted': {
      const job = (data as { job: SessionBackground['jobs'][number] }).job;
      const index = state.jobs.findIndex((old) => old.pid === job.pid);
      if (index >= 0) state.jobs[index] = job;
      else state.jobs.push(job);
      return true;
    }
    case 'job.removed': {
      const { pid } = data as { pid: number };
      state.jobs = state.jobs.filter((job) => job.pid !== pid);
      return true;
    }
    case 'subagent.upserted': {
      const subagent = (data as { subagent: SessionBackground['subagents'][number] }).subagent;
      const index = state.subagents.findIndex((old) => old.runId === subagent.runId);
      if (index >= 0) state.subagents[index] = subagent;
      else state.subagents.push(subagent);
      return true;
    }
    case 'subagent.removed': {
      const { runId } = data as { runId: number };
      state.subagents = state.subagents.filter((run) => run.runId !== runId);
      return true;
    }
    default:
      return false;
  }
}

/** Apply one `artifact.*` frame to a list keyed by `path`. */
export function applyArtifactDelta<T extends { path: string }>(
  artifacts: T[],
  kind: string,
  data: Record<string, unknown>,
): boolean {
  switch (kind) {
    case 'artifact.upserted': {
      const artifact = (data as { artifact: T }).artifact;
      const index = artifacts.findIndex((old) => old.path === artifact.path);
      if (index >= 0) artifacts[index] = artifact;
      else artifacts.push(artifact);
      return true;
    }
    case 'artifact.removed': {
      const { path } = data as { path: string };
      const index = artifacts.findIndex((old) => old.path === path);
      if (index >= 0) artifacts.splice(index, 1);
      return true;
    }
    default:
      return false;
  }
}

interface StreamEntry {
  count: number;
  controller: AbortController;
  handlers: Set<SessionEventHandler>;
}

const streams = new Map<string, StreamEntry>();

async function follow(
  sessionId: string,
  after: string | undefined,
  entry: StreamEntry,
): Promise<void> {
  while (!entry.controller.signal.aborted) {
    try {
      const params = new URLSearchParams();
      if (after) params.set('after', after);
      const suffix = params.size ? `?${params.toString()}` : '';
      const res = await fetch(
        apiSseUrl(`/sessions/${encodeURIComponent(sessionId)}/events${suffix}`),
        {
          headers: { Accept: 'text/event-stream' },
          signal: entry.controller.signal,
        },
      );
      if (!res.ok) {
        if (res.status === 401) markAuthRequired();
        throw new ApiError(
          'internal_error',
          `session event stream failed: ${res.status}`,
          res.status,
        );
      }
      for await (const frame of parseNamedSseStream<Record<string, unknown>>(
        res,
        entry.controller.signal,
      )) {
        if (frame.id) after = frame.id;
        const { event, data } = frame;
        if (!data || typeof data !== 'object') continue;
        for (const handler of entry.handlers) {
          try {
            handler(event, data, frame.id ?? null);
          } catch (error) {
            console.warn('session event handler failed', error);
          }
        }
      }
    } catch (error) {
      if (entry.controller.signal.aborted) return;
      console.warn('session event stream dropped', error);
    }
    if (entry.controller.signal.aborted) return;
    await new Promise((resolve) => setTimeout(resolve, RECONNECT_DELAY_MS));
  }
}

/**
 * Watch `sessionId`'s `/api/sessions/{id}/events` stream, shared by every
 * watcher of the session. `cursor` — the `cursor` the session read
 * reported — resumes the stream where the read left off; it applies only
 * when this watch opens the stream. Returns the unsubscribe.
 */
export function watchSessionEvents(
  sessionId: string,
  cursor: string | undefined,
  handler: SessionEventHandler,
): () => void {
  let entry = streams.get(sessionId);
  const isFirst = !entry;
  if (!entry) {
    entry = { count: 0, controller: new AbortController(), handlers: new Set() };
    streams.set(sessionId, entry);
  }
  entry.count += 1;
  entry.handlers.add(handler);
  if (isFirst) void follow(sessionId, cursor, entry);
  return () => {
    const current = streams.get(sessionId);
    if (!current || !current.handlers.delete(handler)) return;
    current.count -= 1;
    if (current.count <= 0) {
      current.controller.abort();
      streams.delete(sessionId);
    }
  };
}
