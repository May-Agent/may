import { create } from 'zustand';
import {
  type BotApprovalEventPayload,
  type BotApprovalItem,
  type BotChatEventPayload,
  type BotChatMessage,
  type BotDelegateSummary,
  type BotMemoryCommit,
  type BotMemoryEventPayload,
  type BotMemoryNode,
  type BotScheduleItem,
  type BotStateEventPayload,
  type BotSummary,
  type BotTraceEntry,
  type BotTraceEventPayload,
  type CreateBotRequest,
  type UpdateBotRequest,
  createBot,
  deleteBot,
  deleteBotSchedule,
  getBotMemory,
  getBotMemoryHistory,
  listBotApprovals,
  listBotDelegates,
  listBotMessages,
  listBotSchedules,
  listBotTrace,
  listBots,
  resolveBotApproval,
  sendBotMessage,
  stopBotDelegate,
  updateBot,
} from '../api/bots';

interface BotsState {
  bots: BotSummary[];
  selectedBotId: string | null;
  loading: boolean;
  error: string | null;
  messages: Record<string, BotChatMessage[]>;
  trace: Record<string, BotTraceEntry[]>;
  delegates: Record<string, BotDelegateSummary[]>;
  approvals: Record<string, BotApprovalItem[]>;
  schedules: Record<string, BotScheduleItem[]>;
  memoryTree: Record<string, BotMemoryNode | null>;
  memoryHistory: Record<string, BotMemoryCommit[]>;

  loadBots: () => Promise<void>;
  selectBot: (botId: string | null) => Promise<void>;
  refreshBotDetails: (botId: string) => Promise<void>;
  createNewBot: (req: CreateBotRequest) => Promise<BotSummary>;
  patchBot: (botId: string, req: UpdateBotRequest) => Promise<BotSummary>;
  removeBot: (botId: string) => Promise<void>;
  sendMessage: (botId: string, content: string) => Promise<void>;
  stopDelegate: (botId: string, agentId: string) => Promise<void>;
  decideApproval: (
    botId: string,
    requestId: string,
    approved: boolean,
    reason?: string,
  ) => Promise<void>;
  removeSchedule: (botId: string, scheduleId: string) => Promise<void>;

  applyBotStateEvent: (payload: BotStateEventPayload) => void;
  applyBotChatEvent: (payload: BotChatEventPayload) => void;
  applyBotTraceEvent: (payload: BotTraceEventPayload) => void;
  applyBotApprovalEvent: (payload: BotApprovalEventPayload) => void;
  applyBotMemoryEvent: (payload: BotMemoryEventPayload) => void;
}

export const useBotsStore = create<BotsState>((set, get) => ({
  bots: [],
  selectedBotId: null,
  loading: false,
  error: null,
  messages: {},
  trace: {},
  delegates: {},
  approvals: {},
  schedules: {},
  memoryTree: {},
  memoryHistory: {},

  async loadBots() {
    set({ loading: true, error: null });
    try {
      const bots = await listBots();
      const current = get().selectedBotId;
      const nextSelected =
        current && bots.some((b) => b.id === current) ? current : (bots[0]?.id ?? null);
      set({ bots, selectedBotId: nextSelected, loading: false });
      if (nextSelected) {
        await get().refreshBotDetails(nextSelected);
      }
    } catch (err) {
      set({
        loading: false,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  },

  async selectBot(botId) {
    set({ selectedBotId: botId });
    if (botId) {
      await get().refreshBotDetails(botId);
    }
  },

  async refreshBotDetails(botId) {
    try {
      const [messages, trace, delegates, approvals, schedules, memoryTree, memoryHistory] =
        await Promise.all([
          listBotMessages(botId),
          listBotTrace(botId),
          listBotDelegates(botId),
          listBotApprovals(botId),
          listBotSchedules(botId),
          getBotMemory(botId),
          getBotMemoryHistory(botId),
        ]);
      set((state) => ({
        messages: { ...state.messages, [botId]: messages },
        trace: { ...state.trace, [botId]: trace },
        delegates: { ...state.delegates, [botId]: delegates },
        approvals: { ...state.approvals, [botId]: approvals },
        schedules: { ...state.schedules, [botId]: schedules },
        memoryTree: { ...state.memoryTree, [botId]: memoryTree },
        memoryHistory: { ...state.memoryHistory, [botId]: memoryHistory },
      }));
    } catch (err) {
      set({ error: err instanceof Error ? err.message : String(err) });
    }
  },

  async createNewBot(req) {
    const bot = await createBot(req);
    set((state) => ({
      bots: [...state.bots, bot],
      selectedBotId: bot.id,
    }));
    await get().refreshBotDetails(bot.id);
    return bot;
  },

  async patchBot(botId, req) {
    const updated = await updateBot(botId, req);
    set((state) => ({
      bots: state.bots.map((b) => (b.id === updated.id ? updated : b)),
    }));
    return updated;
  },

  async removeBot(botId) {
    await deleteBot(botId);
    set((state) => {
      const bots = state.bots.filter((b) => b.id !== botId);
      return {
        bots,
        selectedBotId: state.selectedBotId === botId ? (bots[0]?.id ?? null) : state.selectedBotId,
      };
    });
  },

  async sendMessage(botId, content) {
    const msg = await sendBotMessage(botId, content);
    set((state) => {
      const existing = state.messages[botId] ?? [];
      if (existing.some((m) => m.id === msg.id)) return state;
      return {
        messages: { ...state.messages, [botId]: [...existing, msg] },
      };
    });
  },

  async stopDelegate(botId, agentId) {
    const updated = await stopBotDelegate(botId, agentId);
    set((state) => ({
      delegates: {
        ...state.delegates,
        [botId]: (state.delegates[botId] ?? []).map((d) =>
          d.agentId === updated.agentId ? updated : d,
        ),
      },
    }));
  },

  async decideApproval(botId, requestId, approved, reason) {
    const updated = await resolveBotApproval(botId, requestId, approved, reason);
    set((state) => ({
      approvals: {
        ...state.approvals,
        [botId]: (state.approvals[botId] ?? []).map((a) =>
          a.requestId === updated.requestId ? updated : a,
        ),
      },
    }));
  },

  async removeSchedule(botId, scheduleId) {
    await deleteBotSchedule(botId, scheduleId);
    set((state) => ({
      schedules: {
        ...state.schedules,
        [botId]: (state.schedules[botId] ?? []).filter((s) => s.id !== scheduleId),
      },
    }));
  },

  applyBotStateEvent(payload) {
    set((state) => {
      const nextSelected =
        state.selectedBotId && payload.bots.some((b) => b.id === state.selectedBotId)
          ? state.selectedBotId
          : (payload.bots[0]?.id ?? null);
      return {
        bots: payload.bots,
        selectedBotId: nextSelected,
      };
    });
  },

  applyBotChatEvent(payload) {
    const msg = payload.message;
    set((state) => {
      const list = state.messages[msg.botId] ?? [];
      if (list.some((m) => m.id === msg.id)) return state;
      return {
        messages: { ...state.messages, [msg.botId]: [...list, msg] },
      };
    });
  },

  applyBotTraceEvent(payload) {
    const entry = payload.entry;
    set((state) => {
      const list = state.trace[entry.botId] ?? [];
      if (list.some((t) => t.id === entry.id)) return state;
      return {
        trace: { ...state.trace, [entry.botId]: [...list, entry] },
      };
    });
  },

  applyBotApprovalEvent(payload) {
    const approval = payload.approval;
    set((state) => {
      const list = state.approvals[approval.botId] ?? [];
      const exists = list.some((a) => a.requestId === approval.requestId);
      const next = exists
        ? list.map((a) => (a.requestId === approval.requestId ? approval : a))
        : [approval, ...list];
      return {
        approvals: { ...state.approvals, [approval.botId]: next },
      };
    });
  },

  applyBotMemoryEvent(payload) {
    const botId = payload.botId;
    if (get().selectedBotId === botId) {
      void Promise.all([getBotMemory(botId), getBotMemoryHistory(botId)]).then(
        ([memoryTree, memoryHistory]) => {
          set((state) => ({
            memoryTree: { ...state.memoryTree, [botId]: memoryTree },
            memoryHistory: { ...state.memoryHistory, [botId]: memoryHistory },
          }));
        },
      );
    }
  },
}));
