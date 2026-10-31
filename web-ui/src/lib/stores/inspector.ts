import { create } from 'zustand';
import {
  cancelSubagentRun,
  getSessionBackground,
  killBackgroundJob,
  type SessionBackground,
} from '../api/background';
import { getSessionTasks } from '../api/tasks';
import { parseTaskTree, type TaskTree } from '../tasks/taskTree';
import {
  NO_STOP_REQUESTS,
  settleStopRequests,
  type StopRequests,
} from '../inspector/model';
import type { SessionPlanState } from '../types';

export interface SessionTasks {
  /** Raw `tasks.md`, kept for the editor; the inspector renders `tree`. */
  content: string;
  tree: TaskTree;
  planState: SessionPlanState;
}

/** Per-session inspector data; sessions never share an entry. */
export interface InspectorEntry {
  tasks: SessionTasks | null;
  tasksLoading: boolean;
  tasksError: string | null;
  background: SessionBackground | null;
  backgroundError: string | null;
  stopping: StopRequests;
}

export const EMPTY_INSPECTOR_ENTRY: InspectorEntry = {
  tasks: null,
  tasksLoading: false,
  tasksError: null,
  background: null,
  backgroundError: null,
  stopping: NO_STOP_REQUESTS,
};

interface InspectorState {
  sessions: Record<string, InspectorEntry>;

  /** Fetch tasks.md; overlapping calls coalesce into one follow-up fetch. */
  loadTasks: (sessionId: string) => Promise<void>;
  /** Store a tasks document the client itself just saved. */
  setTasks: (sessionId: string, content: string, planState: SessionPlanState) => void;
  loadBackground: (sessionId: string) => Promise<void>;
  /** Apply a `session_background` snapshot. */
  applyBackground: (snapshot: SessionBackground) => void;
  killJob: (sessionId: string, pid: number) => Promise<void>;
  stopSubagent: (sessionId: string, runId: number) => Promise<void>;
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export const useInspectorStore = create<InspectorState>((set, get) => {
  const entry = (sessionId: string) => get().sessions[sessionId] ?? EMPTY_INSPECTOR_ENTRY;
  const patch = (sessionId: string, fn: (prev: InspectorEntry) => Partial<InspectorEntry>) =>
    set((state) => {
      const prev = state.sessions[sessionId] ?? EMPTY_INSPECTOR_ENTRY;
      return { sessions: { ...state.sessions, [sessionId]: { ...prev, ...fn(prev) } } };
    });

  /** Sessions with a tasks fetch in flight, and whether another is owed. */
  const inflight = new Map<string, { again: boolean }>();

  const tasksOf = (content: string, planState: SessionPlanState): SessionTasks => ({
    content,
    planState,
    tree: parseTaskTree(content),
  });

  return {
    sessions: {},

    async loadTasks(sessionId) {
      const running = inflight.get(sessionId);
      if (running) {
        running.again = true;
        return;
      }
      const marker = { again: false };
      inflight.set(sessionId, marker);
      patch(sessionId, () => ({ tasksLoading: true }));
      try {
        do {
          marker.again = false;
          try {
            const res = await getSessionTasks(sessionId);
            patch(sessionId, () => ({
              tasks: tasksOf(res.content ?? '', res.planState),
              tasksError: null,
            }));
          } catch (err) {
            patch(sessionId, () => ({ tasksError: message(err) }));
          }
        } while (marker.again);
      } finally {
        inflight.delete(sessionId);
        patch(sessionId, () => ({ tasksLoading: false }));
      }
    },

    setTasks(sessionId, content, planState) {
      patch(sessionId, () => ({ tasks: tasksOf(content, planState), tasksError: null }));
    },

    async loadBackground(sessionId) {
      try {
        const snapshot = await getSessionBackground(sessionId);
        get().applyBackground({ ...snapshot, sessionId: snapshot.sessionId ?? sessionId });
      } catch (err) {
        patch(sessionId, () => ({ backgroundError: message(err) }));
      }
    },

    applyBackground(snapshot) {
      patch(snapshot.sessionId, (prev) => ({
        background: snapshot,
        backgroundError: null,
        stopping: settleStopRequests(prev.stopping, snapshot),
      }));
    },

    async killJob(sessionId, pid) {
      if (entry(sessionId).stopping.jobs.includes(pid)) return;
      patch(sessionId, (prev) => ({
        stopping: { ...prev.stopping, jobs: [...prev.stopping.jobs, pid] },
      }));
      try {
        await killBackgroundJob(sessionId, pid);
      } catch (err) {
        patch(sessionId, (prev) => ({
          stopping: { ...prev.stopping, jobs: prev.stopping.jobs.filter((p) => p !== pid) },
        }));
        throw err;
      }
      // The event stream normally reports the kill; refresh in case it raced.
      await get().loadBackground(sessionId);
    },

    async stopSubagent(sessionId, runId) {
      if (entry(sessionId).stopping.subagents.includes(runId)) return;
      patch(sessionId, (prev) => ({
        stopping: { ...prev.stopping, subagents: [...prev.stopping.subagents, runId] },
      }));
      try {
        await cancelSubagentRun(sessionId, runId);
      } catch (err) {
        patch(sessionId, (prev) => ({
          stopping: {
            ...prev.stopping,
            subagents: prev.stopping.subagents.filter((id) => id !== runId),
          },
        }));
        throw err;
      }
      await get().loadBackground(sessionId);
    },
  };
});
