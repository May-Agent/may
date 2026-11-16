import { afterEach, describe, expect, test } from 'bun:test';
import {
  applyArtifactDelta,
  applyBackgroundDelta,
  watchSessionEvents,
  type BackgroundState,
} from './sessionEvents';
import type { SessionBackground } from './background';

const realFetch = globalThis.fetch;
const encoder = new TextEncoder();

afterEach(() => {
  globalThis.fetch = realFetch;
});

function serve(body: string, seen: string[]) {
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    seen.push(String(input));
    return new Response(encoder.encode(body), { status: 200 });
  }) as typeof fetch;
}

function job(pid: number): SessionBackground['jobs'][number] {
  return {
    pid,
    taskId: 't',
    executionKey: 'k',
    script: 'sleep 1',
    mode: 'sandboxed',
    elapsedMs: 0,
    status: 'running',
  } as unknown as SessionBackground['jobs'][number];
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

describe('watchSessionEvents', () => {
  test('a resumed watch names the cursor as after', async () => {
    const urls: string[] = [];
    serve('', urls);
    const unwatch = watchSessionEvents('s 1', '41.2', () => {});
    await settle();
    unwatch();
    expect(urls[0]).toContain('/api/sessions/s%201/events?after=41.2');
  });

  test('a watch without cursor opens the stream live', async () => {
    const urls: string[] = [];
    serve('', urls);
    const unwatch = watchSessionEvents('s1', undefined, () => {});
    await settle();
    unwatch();
    expect(urls[0]).toContain('/api/sessions/s1/events');
    expect(urls[0]).not.toContain('after=');
  });

  test('each frame reaches every watcher of the session', async () => {
    serve(
      'event: reset\nid: 1.1\ndata: {"sessionId":"s1"}\n\n' +
        'event: warning\nid: 1.2\ndata: {"sessionId":"s1","message":"CI ran"}\n\n',
      [],
    );
    const first: string[] = [];
    const second: string[] = [];
    const unwatchA = watchSessionEvents('s2', undefined, (kind) => first.push(kind));
    const unwatchB = watchSessionEvents('s2', undefined, (kind) => second.push(kind));
    await settle();
    unwatchA();
    unwatchB();
    expect(first).toEqual(['reset', 'warning']);
    expect(second).toEqual(['reset', 'warning']);
  });
});

describe('background deltas', () => {
  test('a job upserted then removed leaves the snapshot without it', () => {
    const state: BackgroundState = { jobs: [], subagents: [] };
    expect(
      applyBackgroundDelta(state, 'job.upserted', { sessionId: 's', job: job(9) }),
    ).toBe(true);
    expect(state.jobs.map((j) => j.pid)).toEqual([9]);
    const changed = job(9);
    (changed as { elapsedMs: number }).elapsedMs = 42;
    applyBackgroundDelta(state, 'job.upserted', { sessionId: 's', job: changed });
    expect(state.jobs).toHaveLength(1);
    expect(state.jobs[0].elapsedMs).toBe(42);
    expect(applyBackgroundDelta(state, 'job.removed', { sessionId: 's', pid: 9 })).toBe(true);
    expect(state.jobs).toHaveLength(0);
  });

  test('a subagent upserted then removed leaves the snapshot without it', () => {
    const state: BackgroundState = { jobs: [], subagents: [] };
    const run = { runId: 3, agent: 'explore', prompt: 'p' } as SessionBackground['subagents'][number];
    applyBackgroundDelta(state, 'subagent.upserted', { sessionId: 's', subagent: run });
    expect(state.subagents.map((r) => r.runId)).toEqual([3]);
    applyBackgroundDelta(state, 'subagent.removed', { sessionId: 's', runId: 3 });
    expect(state.subagents).toHaveLength(0);
  });
});

describe('artifact deltas', () => {
  test('an artifact upserted then removed leaves the list without it', () => {
    const artifacts: { path: string; name: string }[] = [];
    const artifact = { path: 'a/b.html', name: 'b.html' };
    expect(applyArtifactDelta(artifacts, 'artifact.upserted', { artifact })).toBe(true);
    expect(artifacts).toEqual([artifact]);
    const moved = { path: 'a/b.html', name: 'b2.html' };
    applyArtifactDelta(artifacts, 'artifact.upserted', { artifact: moved });
    expect(artifacts).toEqual([moved]);
    expect(applyArtifactDelta(artifacts, 'artifact.removed', { path: 'a/b.html' })).toBe(true);
    expect(artifacts).toHaveLength(0);
  });
});
