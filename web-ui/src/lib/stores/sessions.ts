import { create } from 'zustand';
import type { ApiMessage, ApiSession, SessionRuntimeSummary } from '../types';
import {
  archiveSession,
  createSession,
  deleteSession,
  getSession,
  listSessions,
  updateSession,
  unarchiveSession,
  type ReasoningEffortPatch,
} from '../api/sessions';

interface SessionSummary {
  id: string;
  title: string;
  lastModified: string;
  preview: string;
  profileId: string | null;
  runtime?: SessionRuntimeSummary;
}

interface SessionsState {
  index: Record<string, SessionSummary>;
  order: string[];
  active: ApiSession | null;
  /** Loaded messages keyed by the session that owns them. */
  messages: Record<string, ApiMessage[]>;
  loading: boolean;
  error: string | null;

  load: () => Promise<void>;
  open: (id: string) => Promise<void>;
  refreshActive: (id: string) => Promise<void>;
  createNew: (profileId?: string | null) => Promise<ApiSession>;
  rename: (id: string, title: string) => Promise<void>;
  setModel: (id: string, model: string) => Promise<void>;
  setReasoningEffort: (id: string, effort: ReasoningEffortPatch) => Promise<void>;
  remove: (id: string) => Promise<void>;
  archive: (id: string, archived: boolean) => Promise<void>;

  appendMessage: (sessionId: string, msg: ApiMessage) => void;
  replaceMessage: (sessionId: string, msg: ApiMessage) => void;
  updateRuntime: (id: string, runtime: Partial<SessionRuntimeSummary>) => void;
}

let activeRequestGeneration = 0;

export const useSessionsStore = create<SessionsState>((set, get) => ({
  index: {},
  order: [],
  active: null,
  messages: {},
  loading: false,
  error: null,

  async load() {
    set({ loading: true, error: null });
    try {
      const data = await listSessions();
      const index: Record<string, SessionSummary> = {};
      for (const item of data.items) index[item.id] = item;
      set({
        index,
        order: data.items.map((s) => s.id),
        loading: false,
      });
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },

  async open(id: string) {
    const generation = ++activeRequestGeneration;
    set({ loading: true, error: null });
    try {
      const session = await getSession(id);
      if (generation !== activeRequestGeneration) return;
      set({
        active: session,
        messages: { ...get().messages, [id]: session.messages ?? [] },
        loading: false,
      });
    } catch (err) {
      if (generation !== activeRequestGeneration) return;
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },

  async refreshActive(id: string) {
    const generation = ++activeRequestGeneration;
    try {
      const session = await getSession(id);
      if (generation !== activeRequestGeneration) return;
      set((prev) => {
        if (prev.active?.id !== id) return prev;
        return {
          active: session,
          messages: { ...prev.messages, [id]: session.messages ?? [] },
        };
      });
    } catch (err) {
      if (generation !== activeRequestGeneration) return;
      set({ error: err instanceof Error ? err.message : String(err) });
    }
  },

  async createNew(profileId) {
    activeRequestGeneration += 1;
    const created = await createSession({ profileId: profileId ?? null });
    set((prev) => ({
      index: {
        ...prev.index,
        [created.id]: {
          id: created.id,
          title: created.title,
          lastModified: created.lastModified,
          preview: created.preview,
          profileId: created.profileId,
          runtime: created.runtime,
        },
      },
      order: [created.id, ...prev.order.filter((x) => x !== created.id)],
      active: created,
      messages: { ...prev.messages, [created.id]: created.messages ?? [] },
    }));
    return created;
  },

  async rename(id, title) {
    await updateSession(id, { title });
    set((prev) => {
      const entry = prev.index[id];
      if (!entry) return prev;
      return {
        index: { ...prev.index, [id]: { ...entry, title } },
        active: prev.active?.id === id ? { ...prev.active, title } : prev.active,
      };
    });
  },

  async setModel(id, model) {
    const updated = await updateSession(id, { model });
    set((prev) => ({
      active: prev.active?.id === id ? { ...prev.active, model: updated.model } : prev.active,
    }));
  },

  async setReasoningEffort(id, effort) {
    const updated = await updateSession(id, { reasoningEffort: effort });
    set((prev) => ({
      active:
        prev.active?.id === id
          ? { ...prev.active, reasoningEffort: updated.reasoningEffort ?? null }
          : prev.active,
    }));
  },

  async remove(id) {
    await deleteSession(id);
    activeRequestGeneration += 1;
    set((prev) => {
      const { [id]: _, ...rest } = prev.index;
      const { [id]: _messages, ...messages } = prev.messages;
      return {
        index: rest,
        messages,
        order: prev.order.filter((x) => x !== id),
        active: prev.active?.id === id ? null : prev.active,
      };
    });
  },

  async archive(id, archived) {
    if (archived) await archiveSession(id);
    else await unarchiveSession(id);
    await get().load();
  },

  appendMessage(sessionId, msg) {
    set((prev) => ({
      messages: {
        ...prev.messages,
        [sessionId]: [...(prev.messages[sessionId] ?? []), msg],
      },
    }));
  },

  replaceMessage(sessionId, msg) {
    set((prev) => ({
      messages: {
        ...prev.messages,
        [sessionId]: (prev.messages[sessionId] ?? []).map((m) => (m.id === msg.id ? msg : m)),
      },
    }));
  },

  updateRuntime(id, patch) {
    set((prev) => {
      const entry = prev.index[id];
      if (!entry) return prev;
      const nextRuntime: SessionRuntimeSummary = { ...(entry.runtime ?? {}), ...patch };
      const active =
        prev.active?.id === id
          ? { ...prev.active, runtime: { ...prev.active.runtime, ...patch } }
          : prev.active;
      return {
        index: { ...prev.index, [id]: { ...entry, runtime: nextRuntime } },
        active,
      };
    });
  },
}));
