import { create } from 'zustand';
import type {
  ApiMessage,
  ApiSession,
  SessionPlanState,
  SessionRuntimeSummary,
} from '../types';
import {
  archiveSession,
  createSession,
  deleteSession,
  getSession,
  listSessions,
  updateSession,
  unarchiveSession,
  type ReasoningEffortPatch,
  type UpdateSessionPatch,
} from '../api/sessions';
import { putSessionTasks } from '../api/tasks';
import { useConfigStore } from './config';

const DRAFT_PREFIX = 'draft_';
const LOCAL_PREFIX = 'local-';

/**
 * `fetched` with this client's not-yet-persisted sends re-appended: a
 * refresh that ran before the daemon committed a just-sent message must
 * not drop its optimistic copy. A fetched user message with equal content
 * consumes one pending echo.
 */
function mergePendingLocal(fetched: ApiMessage[], current: ApiMessage[]): ApiMessage[] {
  const echoes = new Map<string, number>();
  for (const msg of fetched) {
    if (msg.role === 'user') echoes.set(msg.content, (echoes.get(msg.content) ?? 0) + 1);
  }
  const pending: ApiMessage[] = [];
  for (const msg of current) {
    if (!msg.id.startsWith(LOCAL_PREFIX) || msg.role !== 'user') continue;
    const seen = echoes.get(msg.content) ?? 0;
    if (seen > 0) echoes.set(msg.content, seen - 1);
    else pending.push(msg);
  }
  return pending.length === 0 ? fetched : [...fetched, ...pending];
}

/**
 * Whether `id` names a client-side draft session — a "new chat" that has not
 * sent its first message and does not exist on the daemon yet. Draft ids can
 * never collide with daemon-issued petname ids (lowercase words + hyphens).
 */
export function isDraftSessionId(id: string): boolean {
  return id.startsWith(DRAFT_PREFIX);
}

interface SessionSummary {
  id: string;
  title: string;
  lastModified: string;
  preview: string;
  profileId: string | null;
  runtime?: SessionRuntimeSummary;
}

/**
 * A session that exists only on this client. Settings the user applies to it
 * (title, model, effort, plan) are captured here and applied to the real
 * session when the first message materializes it.
 */
interface DraftRecord {
  session: ApiSession;
  /** `tasks.md` content staged while the session was still a draft. */
  planContent: string | null;
}

interface SessionsState {
  index: Record<string, SessionSummary>;
  order: string[];
  active: ApiSession | null;
  /** Client-side new chats, keyed by their draft id. */
  drafts: Record<string, DraftRecord>;
  /** Loaded messages keyed by the session that owns them. */
  messages: Record<string, ApiMessage[]>;
  loading: boolean;
  error: string | null;

  load: () => Promise<void>;
  open: (id: string) => Promise<void>;
  refreshActive: (id: string) => Promise<void>;
  createNew: (profileId?: string | null, opts?: { repo?: string }) => Promise<ApiSession>;
  /**
   * Turn a draft into a real daemon session: `POST /api/sessions`, apply any
   * staged settings, rekey the stores from the draft id to the real id.
   */
  materialize: (id: string) => Promise<ApiSession>;
  /** Stage a plan on a draft without touching the daemon. */
  saveDraftPlan: (id: string, content: string, planState: SessionPlanState) => void;
  rename: (id: string, title: string) => Promise<void>;
  setModel: (id: string, model: string) => Promise<void>;
  setReasoningEffort: (id: string, effort: ReasoningEffortPatch) => Promise<void>;
  remove: (id: string) => Promise<void>;
  archive: (id: string, archived: boolean) => Promise<void>;

  appendMessage: (sessionId: string, msg: ApiMessage) => void;
  replaceMessage: (sessionId: string, msg: ApiMessage) => void;
  /** Drop the latest user message whose text is `content`: a withdrawn turn's. */
  withdrawUserMessage: (sessionId: string, content: string) => void;
  updateRuntime: (id: string, runtime: Partial<SessionRuntimeSummary>) => void;
}

let activeRequestGeneration = 0;

export const useSessionsStore = create<SessionsState>((set, get) => ({
  index: {},
  order: [],
  active: null,
  drafts: {},
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
    const draft = get().drafts[id];
    if (draft) {
      activeRequestGeneration += 1;
      set((prev) => ({
        active: draft.session,
        messages: { ...prev.messages, [id]: prev.messages[id] ?? [] },
        loading: false,
      }));
      return;
    }
    const generation = ++activeRequestGeneration;
    set({ loading: true, error: null });
    try {
      const session = await getSession(id);
      if (generation !== activeRequestGeneration) return;
      set({
        active: session,
        messages: {
          ...get().messages,
          [id]: mergePendingLocal(session.messages ?? [], get().messages[id] ?? []),
        },
        loading: false,
      });
    } catch (err) {
      if (generation !== activeRequestGeneration) return;
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },

  async refreshActive(id: string) {
    if (isDraftSessionId(id)) return;
    const generation = ++activeRequestGeneration;
    try {
      const session = await getSession(id);
      if (generation !== activeRequestGeneration) return;
      set((prev) => {
        if (prev.active?.id !== id) return prev;
        return {
          active: session,
          messages: {
            ...prev.messages,
            [id]: mergePendingLocal(session.messages ?? [], prev.messages[id] ?? []),
          },
        };
      });
    } catch (err) {
      if (generation !== activeRequestGeneration) return;
      set({ error: err instanceof Error ? err.message : String(err) });
    }
  },

  async createNew(profileId, opts) {
    activeRequestGeneration += 1;
    const draft: ApiSession = {
      id: `${DRAFT_PREFIX}${crypto.randomUUID()}`,
      title: 'New Chat',
      lastModified: new Date().toISOString(),
      preview: '',
      messages: [],
      profileId: profileId ?? null,
      model: useConfigStore.getState().config?.fullConfig.lastModel ?? '',
      reasoningEffort: null,
      runtime: {},
      repo: opts?.repo ?? null,
    };
    set((prev) => ({
      drafts: { ...prev.drafts, [draft.id]: { session: draft, planContent: null } },
      active: draft,
      messages: { ...prev.messages, [draft.id]: [] },
    }));
    return draft;
  },

  async materialize(id) {
    const record = get().drafts[id];
    if (!record) throw new Error(`session ${id} is not a draft`);
    let session = await createSession({
      profileId: record.session.profileId,
      model: record.session.model || undefined,
      repo: record.session.repo ?? undefined,
    });
    const patch: UpdateSessionPatch = {};
    if (record.session.title !== 'New Chat') patch.title = record.session.title;
    if (record.session.reasoningEffort != null)
      patch.reasoningEffort = record.session.reasoningEffort as ReasoningEffortPatch;
    if (patch.title !== undefined || patch.reasoningEffort !== undefined) {
      session = await updateSession(session.id, patch);
    }
    const planState = record.session.runtime?.planState;
    if (record.planContent != null || (planState != null && planState !== 'disabled')) {
      await putSessionTasks(session.id, {
        content: record.planContent ?? '',
        planState: planState ?? 'draft',
      });
    }
    activeRequestGeneration += 1;
    set((prev) => {
      const { [id]: _draft, ...drafts } = prev.drafts;
      const messages = { ...prev.messages, [session.id]: prev.messages[id] ?? [] };
      delete messages[id];
      return {
        drafts,
        messages,
        active: prev.active?.id === id ? session : prev.active,
        index: {
          ...prev.index,
          [session.id]: {
            id: session.id,
            title: session.title,
            lastModified: session.lastModified,
            preview: session.preview,
            profileId: session.profileId,
            runtime: session.runtime,
          },
        },
        order: [session.id, ...prev.order.filter((x) => x !== session.id)],
      };
    });
    return session;
  },

  saveDraftPlan(id, content, planState) {
    set((prev) => {
      const draft = prev.drafts[id];
      if (!draft) return prev;
      const session = {
        ...draft.session,
        runtime: { ...draft.session.runtime, planState },
      };
      return {
        drafts: { ...prev.drafts, [id]: { session, planContent: content } },
        active: prev.active?.id === id ? session : prev.active,
      };
    });
  },

  async rename(id, title) {
    const draft = get().drafts[id];
    if (draft) {
      set((prev) => ({
        drafts: {
          ...prev.drafts,
          [id]: { ...draft, session: { ...draft.session, title } },
        },
        active: prev.active?.id === id ? { ...prev.active, title } : prev.active,
      }));
      return;
    }
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
    const draft = get().drafts[id];
    if (draft) {
      set((prev) => ({
        drafts: {
          ...prev.drafts,
          [id]: { ...draft, session: { ...draft.session, model } },
        },
        active: prev.active?.id === id ? { ...prev.active, model } : prev.active,
      }));
      return;
    }
    const updated = await updateSession(id, { model });
    set((prev) => ({
      active: prev.active?.id === id ? { ...prev.active, model: updated.model } : prev.active,
    }));
  },

  async setReasoningEffort(id, effort) {
    const draft = get().drafts[id];
    if (draft) {
      set((prev) => ({
        drafts: {
          ...prev.drafts,
          [id]: { ...draft, session: { ...draft.session, reasoningEffort: effort } },
        },
        active:
          prev.active?.id === id
            ? { ...prev.active, reasoningEffort: effort }
            : prev.active,
      }));
      return;
    }
    const updated = await updateSession(id, { reasoningEffort: effort });
    set((prev) => ({
      active:
        prev.active?.id === id
          ? { ...prev.active, reasoningEffort: updated.reasoningEffort ?? null }
          : prev.active,
    }));
  },

  async remove(id) {
    if (get().drafts[id]) {
      activeRequestGeneration += 1;
      set((prev) => {
        const { [id]: _draft, ...drafts } = prev.drafts;
        const { [id]: _messages, ...messages } = prev.messages;
        return {
          drafts,
          messages,
          active: prev.active?.id === id ? null : prev.active,
        };
      });
      return;
    }
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

  withdrawUserMessage(sessionId, content) {
    set((prev) => {
      const messages = prev.messages[sessionId] ?? [];
      const at = messages.findLastIndex((m) => m.role === 'user' && m.content === content);
      if (at < 0) return prev;
      return {
        messages: { ...prev.messages, [sessionId]: messages.filter((_, i) => i !== at) },
      };
    });
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
      const draft = prev.drafts[id];
      const entry = prev.index[id];
      return {
        drafts: draft
          ? {
              ...prev.drafts,
              [id]: {
                ...draft,
                session: {
                  ...draft.session,
                  runtime: { ...draft.session.runtime, ...patch },
                },
              },
            }
          : prev.drafts,
        index: entry
          ? {
              ...prev.index,
              [id]: { ...entry, runtime: { ...(entry.runtime ?? {}), ...patch } },
            }
          : prev.index,
        active:
          prev.active?.id === id
            ? { ...prev.active, runtime: { ...prev.active.runtime, ...patch } }
            : prev.active,
      };
    });
  },
}));
