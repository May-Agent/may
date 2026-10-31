import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test';
import type { BackgroundJob, SessionBackground, SubagentRun } from '../api/background';
import { useInspectorStore, EMPTY_INSPECTOR_ENTRY } from './inspector';
import { useTurnStore } from './turn';
import {
  formatElapsed,
  formatTokens,
  orderJobs,
  scriptHeadline,
  settleStopRequests,
} from '../inspector/model';

const originalFetch = globalThis.fetch;
const encoder = new TextEncoder();

function job(pid: number, state: BackgroundJob['status']['state'], elapsedMs = 1000): BackgroundJob {
  const status =
    state === 'completed'
      ? { state, exit_code: 0 }
      : state === 'failed'
        ? { state, error: 'boom' }
        : { state };
  return {
    pid,
    taskId: `task-${pid}`,
    script: `sleep ${pid}`,
    mode: 'sandboxed',
    elapsedMs,
    status: status as BackgroundJob['status'],
  };
}

function run(runId: number): SubagentRun {
  return {
    runId,
    agent: 'explore',
    prompt: 'look around',
    elapsedMs: 2000,
    promptTokens: 1200,
    completionTokens: 300,
    requests: 2,
  };
}

function snapshot(sessionId: string, jobs: BackgroundJob[], subagents: SubagentRun[] = []): SessionBackground {
  return { sessionId, jobs, subagents };
}

interface Calls {
  urls: string[];
  methods: string[];
}

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function installFetch(handler: (url: string, method: string) => Response | Promise<Response>): Calls {
  const calls: Calls = { urls: [], methods: [] };
  globalThis.fetch = mock(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? 'GET';
    calls.urls.push(url);
    calls.methods.push(method);
    return handler(url, method);
  }) as unknown as typeof fetch;
  return calls;
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

beforeEach(() => {
  useInspectorStore.setState({ sessions: {} });
  useTurnStore.setState({ turns: {} });
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('inspector model', () => {
  test('stop requests settle once the job stops or the run disappears', () => {
    const requests = { jobs: [1, 2], subagents: [7, 8] };
    const next = settleStopRequests(
      requests,
      snapshot('S', [job(1, 'running'), job(2, 'killed')], [run(7)]),
    );
    expect(next).toEqual({ jobs: [1], subagents: [7] });
    // Nothing settled: the same object comes back, so subscribers stay quiet.
    expect(settleStopRequests(next, snapshot('S', [job(1, 'running')], [run(7)]))).toBe(next);
  });

  test('running jobs come first, newest first', () => {
    const ordered = orderJobs([
      job(1, 'completed', 50),
      job(2, 'running', 9000),
      job(3, 'running', 100),
      job(4, 'killed', 10),
    ]);
    expect(ordered.map((j) => j.pid)).toEqual([3, 2, 4, 1]);
  });

  test('formats durations, tokens and script headlines', () => {
    expect(formatElapsed(999)).toBe('0s');
    expect(formatElapsed(45_000)).toBe('45s');
    expect(formatElapsed(192_000)).toBe('3m 12s');
    expect(formatElapsed(3_840_000)).toBe('1h 04m');
    expect(formatTokens(950)).toBe('950');
    expect(formatTokens(1234)).toBe('1.2k');
    expect(formatTokens(56_789)).toBe('57k');
    expect(formatTokens(1_250_000)).toBe('1.3M');
    expect(scriptHeadline('\n  cargo test \\\n  --all\n')).toBe('cargo test \\');
  });
});

describe('inspector store', () => {
  test('tasks load into a tree without the notes', async () => {
    installFetch((url) => {
      expect(url).toBe('/api/sessions/A/tasks');
      return json({ content: '# Plan\nSome notes.\n- [x] One\n  - [ ] Two (#4)\n', planState: 'approved' });
    });
    await useInspectorStore.getState().loadTasks('A');
    const entry = useInspectorStore.getState().sessions.A;
    expect(entry.tasksLoading).toBe(false);
    expect(entry.tasks?.planState).toBe('approved');
    expect(entry.tasks?.tree.total).toBe(2);
    expect(entry.tasks?.tree.groups[0].tasks[0].children[0].issues[0].label).toBe('#4');
    expect(JSON.stringify(entry.tasks?.tree)).not.toContain('Some notes');
  });

  test('overlapping task loads coalesce into one follow-up fetch', async () => {
    let served = 0;
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => (release = resolve));
    const calls = installFetch(async () => {
      served += 1;
      if (served === 1) await gate;
      return json({ content: `- [ ] v${served}`, planState: 'draft' });
    });
    const first = useInspectorStore.getState().loadTasks('A');
    void useInspectorStore.getState().loadTasks('A');
    void useInspectorStore.getState().loadTasks('A');
    expect(useInspectorStore.getState().sessions.A.tasksLoading).toBe(true);
    release();
    await first;
    expect(calls.urls).toHaveLength(2);
    expect(useInspectorStore.getState().sessions.A.tasks?.tree.groups[0].tasks[0].text).toBe('v2');
  });

  test('a failed tasks load keeps the last document and reports the error', async () => {
    useInspectorStore.getState().setTasks('A', '- [ ] kept', 'draft');
    installFetch(() => new Response('{"message":"nope"}', { status: 500 }));
    await useInspectorStore.getState().loadTasks('A');
    const entry = useInspectorStore.getState().sessions.A;
    expect(entry.tasks?.content).toBe('- [ ] kept');
    expect(entry.tasksError).toBe('nope');
  });

  test('snapshots are kept per session', () => {
    const store = useInspectorStore.getState();
    store.applyBackground(snapshot('A', [job(1, 'running')]));
    store.applyBackground(snapshot('B', [], [run(3)]));
    const { sessions } = useInspectorStore.getState();
    expect(sessions.A.background?.jobs.map((j) => j.pid)).toEqual([1]);
    expect(sessions.B.background?.subagents.map((s) => s.runId)).toEqual([3]);
    expect(sessions.C ?? EMPTY_INSPECTOR_ENTRY).toEqual(EMPTY_INSPECTOR_ENTRY);
  });

  test('killing a job marks it stopping until a snapshot shows it stopped', async () => {
    useInspectorStore.getState().applyBackground(snapshot('A', [job(42, 'running')]));
    let killed = false;
    const calls = installFetch((url, method) => {
      if (url === '/api/sessions/A/jobs/42/kill' && method === 'POST') {
        killed = true;
        return json({ ok: true });
      }
      if (url === '/api/sessions/A/background') {
        // The daemon has not reaped it yet on the first refresh.
        return json(snapshot('A', [job(42, 'running')]));
      }
      throw new Error(`unexpected ${method} ${url}`);
    });
    await useInspectorStore.getState().killJob('A', 42);
    expect(killed).toBe(true);
    expect(useInspectorStore.getState().sessions.A.stopping.jobs).toEqual([42]);
    // A second click while stopping sends nothing.
    await useInspectorStore.getState().killJob('A', 42);
    expect(calls.urls.filter((u) => u.endsWith('/kill'))).toHaveLength(1);
    // The event stream reports it killed.
    useInspectorStore.getState().applyBackground(snapshot('A', [job(42, 'killed')]));
    expect(useInspectorStore.getState().sessions.A.stopping.jobs).toEqual([]);
  });

  test('a failed kill clears the stopping state and rethrows', async () => {
    useInspectorStore.getState().applyBackground(snapshot('A', [job(5, 'running')]));
    installFetch(() => new Response('{"message":"gone"}', { status: 400 }));
    await expect(useInspectorStore.getState().killJob('A', 5)).rejects.toThrow('gone');
    expect(useInspectorStore.getState().sessions.A.stopping.jobs).toEqual([]);
  });

  test('stopping a subagent run uses DELETE and settles when the run is gone', async () => {
    useInspectorStore.getState().applyBackground(snapshot('A', [], [run(9)]));
    const calls = installFetch((url, method) => {
      if (url === '/api/sessions/A/subagents/9' && method === 'DELETE') return json({ stopped: 1 });
      if (url === '/api/sessions/A/background') return json(snapshot('A', [], []));
      throw new Error(`unexpected ${method} ${url}`);
    });
    await useInspectorStore.getState().stopSubagent('A', 9);
    expect(calls.methods).toEqual(['DELETE', 'GET']);
    const entry = useInspectorStore.getState().sessions.A;
    expect(entry.background?.subagents).toEqual([]);
    expect(entry.stopping.subagents).toEqual([]);
  });

  test('session_background frames on the turn stream reach the inspector', async () => {
    let controller: ReadableStreamDefaultController<Uint8Array> | null = null;
    let topics = '';
    installFetch((url) => {
      if (url.startsWith('/api/events?')) {
        topics = new URLSearchParams(url.slice(url.indexOf('?') + 1)).get('topics') ?? '';
        return new Response(
          new ReadableStream<Uint8Array>({
            start(c) {
              controller = c;
            },
          }),
          { status: 200 },
        );
      }
      throw new Error(`unexpected ${url}`);
    });
    const unwatch = useTurnStore.getState().watch('A');
    await settle();
    expect(topics.split(',')).toEqual(['session_turn', 'session_background']);
    const send = (sessionId: string, jobs: BackgroundJob[]) =>
      controller!.enqueue(
        encoder.encode(
          `event: session_background\ndata: ${JSON.stringify(snapshot(sessionId, jobs))}\n\n`,
        ),
      );
    send('A', [job(1, 'running')]);
    // A frame for another session is ignored.
    send('B', [job(2, 'running')]);
    await settle();
    unwatch();
    const { sessions } = useInspectorStore.getState();
    expect(sessions.A.background?.jobs.map((j) => j.pid)).toEqual([1]);
    expect(sessions.B).toBeUndefined();
  });
});
