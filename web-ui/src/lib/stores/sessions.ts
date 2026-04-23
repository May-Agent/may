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
  activeMessages: ApiMessage[];
  loading: boolean;
  error: string | null;

  load: () => Promise<void>;
  open: (id: string) => Promise<void>;
  createNew: (profileId?: string | null) => Promise<ApiSession>;
  rename: (id: string, title: string) => Promise<void>;
  setModel: (id: string, model: string) => Promise<void>;
  setReasoningEffort: (id: string, effort: ReasoningEffortPatch) => Promise<void>;
  remove: (id: string) => Promise<void>;
  archive: (id: string, archived: boolean) => Promise<void>;

  appendMessage: (msg: ApiMessage) => void;
  replaceMessage: (msg: ApiMessage) => void;
  updateRuntime: (id: string, runtime: Partial<SessionRuntimeSummary>) => void;
}

export const useSessionsStore = create<SessionsState>((set, get) => ({
  index: {},
  order: [],
  active: null,
  activeMessages: [],
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
    set({ loading: true, error: null });
    try {
      const session = await getSession(id);
      set({
        active: session,
        activeMessages: session.messages ?? [],
        loading: false,
      });
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },

  async createNew(profileId) {
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
      activeMessages: created.messages ?? [],
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
    set((prev) => {
      const { [id]: _, ...rest } = prev.index;
      return {
        index: rest,
        order: prev.order.filter((x) => x !== id),
        active: prev.active?.id === id ? null : prev.active,
        activeMessages: prev.active?.id === id ? [] : prev.activeMessages,
      };
    });
  },

  async archive(id, archived) {
    if (archived) await archiveSession(id);
    else await unarchiveSession(id);
    await get().load();
  },

  appendMessage(msg) {
    set((prev) => ({
      activeMessages: [...prev.activeMessages, msg],
    }));
  },

  replaceMessage(msg) {
    set((prev) => ({
      activeMessages: prev.activeMessages.map((m) => (m.id === msg.id ? msg : m)),
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
