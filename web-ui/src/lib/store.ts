import { create } from 'zustand';
import { v4 as uuidv4 } from 'uuid';
import type { Session, Message, Project, Attachment } from '@/types';
import * as api from '@/lib/api';
import { buildModelCatalog, DEFAULT_MODELS, type Model } from '@/lib/constants';

interface AppState {
  sessions: Session[];
  currentSessionId: string | null;
  projects: Project[];
  models: Model[];
  settings: {
    font: string;
    theme: 'light' | 'dark' | 'system';
  };
  loaded: boolean;

  uiConfig: {
    sshServers: Array<{ name: string; target: string }>;
    maxIterations: number;
    fullConfig: api.UiFullConfig | null;
  };
  mcpServers: api.McpServerInfo[];
  skills: api.SkillInfo[];

  loadInitialData: () => Promise<void>;
  createSession: (model?: string) => Promise<string>;
  setCurrentSession: (sessionId: string | null) => void;
  addMessage: (
    sessionId: string,
    content: string,
    role: 'user' | 'assistant',
    attachments?: Attachment[],
  ) => void;
  deleteSession: (sessionId: string) => Promise<void>;
  updateSessionTitle: (sessionId: string, title: string) => Promise<void>;
  updateSessionModel: (sessionId: string, model: string) => Promise<void>;
  updateSessionReasoningEffort: (
    sessionId: string,
    effort: 'low' | 'medium' | 'high',
  ) => void;
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void;

  updateSettings: (settings: Partial<AppState['settings']>) => void;
  searchSessions: (query: string) => Session[];

  refreshRuntimeSettings: () => Promise<void>;
  refreshFullConfig: () => Promise<void>;
  saveFullConfig: (config: api.UiFullConfig) => Promise<void>;
  addSshServer: (name: string, target: string) => Promise<void>;
  removeSshServer: (name: string) => Promise<void>;

  memoryContent: string;
  refreshMemory: () => Promise<void>;
  saveMemory: (content: string) => Promise<void>;
  deleteAllSessions: () => Promise<void>;

  refreshMcpServers: () => Promise<void>;
  addMcpServer: (input: { name: string; command?: string; args?: string[]; url?: string }) => Promise<void>;
  removeMcpServer: (name: string) => Promise<void>;
  reconnectMcpServer: (name: string) => Promise<void>;

  refreshSkills: () => Promise<void>;
  installSkill: (repo: string) => Promise<void>;
  deleteSkill: (dirName: string) => Promise<void>;
}

const MOCK_PROJECTS: Project[] = [
  {
    id: 'proj-1',
    name: 'Personal',
    description: 'Personal projects',
    createdAt: new Date().toISOString(),
  },
];

function sortSessions(sessions: Session[]): Session[] {
  return [...sessions].sort(
    (a, b) =>
      new Date(b.lastModified).getTime() - new Date(a.lastModified).getTime(),
  );
}

export const useStore = create<AppState>((set, get) => ({
  sessions: [],
  currentSessionId: null,
  projects: MOCK_PROJECTS,
  models: DEFAULT_MODELS,
  settings: { font: 'Inter', theme: 'system' },
  loaded: false,

  uiConfig: {
    sshServers: [],
    maxIterations: 50,
    fullConfig: null,
  },
  memoryContent: '',
  mcpServers: [],
  skills: [],

  loadInitialData: async () => {
    if (get().loaded) return;

    const [
      sessionsRes,
      modelIdsRes,
      uiConfigRes,
      fullConfigRes,
      memoryRes,
      mcpServersRes,
      skillsRes,
    ] = await Promise.allSettled([
      api.fetchSessions(),
      api.fetchModels(),
      api.fetchUiConfig(),
      api.fetchFullConfig(),
      api.fetchMemory(),
      api.fetchMcpServers(),
      api.fetchSkills(),
    ]);

    const sessions = sessionsRes.status === 'fulfilled' ? sessionsRes.value : [];
    const modelIds = modelIdsRes.status === 'fulfilled' ? modelIdsRes.value : [];
    const uiConfig =
      uiConfigRes.status === 'fulfilled'
        ? uiConfigRes.value
        : { ssh_servers: [], max_iterations: 50 };
    const fullConfig = fullConfigRes.status === 'fulfilled' ? fullConfigRes.value : null;
    const memoryContent = memoryRes.status === 'fulfilled' ? memoryRes.value : '';
    const mcpServers = mcpServersRes.status === 'fulfilled' ? mcpServersRes.value : [];
    const skills = skillsRes.status === 'fulfilled' ? skillsRes.value : [];

    const sorted = sortSessions(sessions);
    set({
      sessions: sorted,
      models: buildModelCatalog(modelIds),
      uiConfig: {
        sshServers: uiConfig.ssh_servers,
        maxIterations: uiConfig.max_iterations,
        fullConfig,
      },
      memoryContent,
      mcpServers,
      skills,
      loaded: true,
      currentSessionId: null,
    });
  },

  createSession: async (model) => {
    const session = await api.createSession(model);
    set((state) => ({
      sessions: sortSessions([session, ...state.sessions]),
      currentSessionId: session.id,
    }));
    return session.id;
  },

  setCurrentSession: (sessionId) => set({ currentSessionId: sessionId }),

  addMessage: (sessionId, content, role, attachments = []) => {
    const newMessage: Message = {
      id: uuidv4(),
      role,
      content,
      timestamp: new Date().toISOString(),
      attachments,
    };

    set((state) => ({
      sessions: sortSessions(
        state.sessions.map((session) =>
          session.id === sessionId
            ? {
                ...session,
                messages: [...session.messages, newMessage],
                lastModified: new Date().toISOString(),
                preview:
                  content.substring(0, 100) + (content.length > 100 ? '...' : ''),
                title:
                  session.messages.length === 0 && role === 'user'
                    ? content.substring(0, 30)
                    : session.title,
              }
            : session,
        ),
      ),
    }));
  },

  deleteSession: async (sessionId) => {
    await api.removeSession(sessionId);
    set((state) => {
      const sessions = state.sessions.filter((s) => s.id !== sessionId);
      const nextId =
        state.currentSessionId === sessionId
          ? (sessions[0]?.id ?? null)
          : state.currentSessionId;
      return { sessions, currentSessionId: nextId };
    });
  },

  updateSessionTitle: async (sessionId, title) => {
    const updated = await api.updateSession(sessionId, { title });
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, title: updated.title } : s,
      ),
    }));
  },

  updateSessionModel: async (sessionId, model) => {
    const updated = await api.updateSession(sessionId, { model });
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, model: updated.model } : s,
      ),
    }));
  },

  updateSessionReasoningEffort: (sessionId, effort) => {
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === sessionId ? { ...s, reasoningEffort: effort } : s,
      ),
    }));
  },

  replaceSessionMessages: (sessionId, messages) => {
    set((state) => ({
      sessions: sortSessions(
        state.sessions.map((s) =>
          s.id === sessionId
            ? {
                ...s,
                messages,
                lastModified: new Date().toISOString(),
                preview: messages.length
                  ? messages[messages.length - 1].content.slice(0, 100)
                  : s.preview,
              }
            : s,
        ),
      ),
    }));
  },

  updateSettings: (settings) => {
    set((state) => ({ settings: { ...state.settings, ...settings } }));
  },

  searchSessions: (query) => {
    const lower = query.toLowerCase();
    const sessions = get().sessions;
    if (!query) return sessions;
    return sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(lower) ||
        s.messages.some((m) => m.content.toLowerCase().includes(lower)),
    );
  },

  refreshRuntimeSettings: async () => {
    const config = await api.fetchUiConfig();
    set((state) => ({
      uiConfig: {
        ...state.uiConfig,
        sshServers: config.ssh_servers,
        maxIterations: config.max_iterations,
      },
    }));
  },

  refreshFullConfig: async () => {
    const fullConfig = await api.fetchFullConfig();
    set((state) => ({ uiConfig: { ...state.uiConfig, fullConfig } }));
  },

  saveFullConfig: async (fullConfig) => {
    await api.saveFullConfig(fullConfig);
    set((state) => ({ uiConfig: { ...state.uiConfig, fullConfig } }));
  },

  addSshServer: async (name, target) => {
    await api.addSshServer(name, target);
    await get().refreshRuntimeSettings();
  },

  removeSshServer: async (name) => {
    await api.removeSshServer(name);
    await get().refreshRuntimeSettings();
  },

  refreshMemory: async () => {
    const memoryContent = await api.fetchMemory();
    set({ memoryContent });
  },

  saveMemory: async (content) => {
    await api.saveMemory(content);
    set({ memoryContent: content });
  },

  deleteAllSessions: async () => {
    await api.deleteAllSessions();
    const sessions = await api.fetchSessions();
    const sorted = sortSessions(sessions);
    set({
      sessions: sorted,
      currentSessionId: sorted[0]?.id ?? null,
    });
  },

  refreshMcpServers: async () => {
    const mcpServers = await api.fetchMcpServers();
    set({ mcpServers });
  },

  addMcpServer: async (input) => {
    await api.addMcpServer(input);
    await get().refreshMcpServers();
  },

  removeMcpServer: async (name) => {
    await api.removeMcpServer(name);
    await get().refreshMcpServers();
  },

  reconnectMcpServer: async (name) => {
    await api.reconnectMcpServer(name);
    await get().refreshMcpServers();
  },

  refreshSkills: async () => {
    const skills = await api.fetchSkills();
    set({ skills });
  },

  installSkill: async (repo) => {
    await api.installSkill(repo);
    await get().refreshSkills();
  },

  deleteSkill: async (dirName) => {
    await api.deleteSkill(dirName);
    await get().refreshSkills();
  },
}));
