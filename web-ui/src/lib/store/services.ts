import type { StateCreator } from 'zustand';
import type { ScheduledTask } from '@/types';
import * as api from '@/lib/api';
import type { AppState } from './index';
import { TASK_REFRESH_DELAY_MS } from './shared';

export interface ServicesSlice {
  mcpServers: api.McpServerInfo[];
  skills: api.SkillInfo[];
  scheduledTasks: ScheduledTask[];
  pushEnabled: boolean;

  refreshMcpServers: () => Promise<void>;
  addMcpServer: (input: { name: string; command?: string; args?: string[]; url?: string }) => Promise<void>;
  removeMcpServer: (name: string) => Promise<void>;
  reconnectMcpServer: (name: string) => Promise<void>;

  refreshSkills: () => Promise<void>;
  installSkill: (repo: string) => Promise<void>;
  deleteSkill: (dirName: string) => Promise<void>;

  enablePush: () => Promise<void>;
  disablePush: () => Promise<void>;
  sendTestNotification: () => Promise<void>;

  refreshTasks: () => Promise<void>;
  createTask: (input: {
    profileId: string;
    sessionId?: string;
    name: string;
    triggerKind: 'time';
    triggerType: 'once' | 'cron';
    cronExpression?: string;
    runAt?: string;
    scriptPath?: string;
    scriptInline?: string;
    needsNetwork?: boolean;
  }) => Promise<ScheduledTask>;
  updateTask: (id: string, input: {
    name?: string;
    enabled?: boolean;
    cronExpression?: string;
    runAt?: string;
    scriptPath?: string;
    scriptInline?: string;
  }) => Promise<void>;
  deleteTask: (id: string) => Promise<void>;
  approveTask: (id: string) => Promise<void>;
  runTask: (id: string) => Promise<void>;
}

export const createServicesSlice: StateCreator<AppState, [], [], ServicesSlice> = (set, get) => ({
  mcpServers: [],
  skills: [],
  scheduledTasks: [],
  pushEnabled: false,

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

  enablePush: async () => {
    const vapid = await api.fetchVapidKey();
    const registration = await navigator.serviceWorker.ready;

    const raw = atob(vapid.publicKey.replace(/-/g, '+').replace(/_/g, '/'));
    const key = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) key[i] = raw.charCodeAt(i);

    const subscription = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: key,
    });

    const subJson = subscription.toJSON();
    await api.subscribePush({
      endpoint: subJson.endpoint!,
      p256dh: subJson.keys!.p256dh!,
      auth: subJson.keys!.auth!,
    });
    set({ pushEnabled: true });
  },

  disablePush: async () => {
    const registration = await navigator.serviceWorker.ready;
    const subscription = await registration.pushManager.getSubscription();
    if (subscription) {
      await api.unsubscribePush(subscription.endpoint);
      await subscription.unsubscribe();
    }
    set({ pushEnabled: false });
  },

  sendTestNotification: async () => {
    await api.sendTestNotification();
  },

  refreshTasks: async () => {
    const scheduledTasks = await api.fetchTasks();
    set({ scheduledTasks });
  },

  createTask: async (input) => {
    const task = await api.createTask(input);
    await get().refreshTasks();
    return task;
  },

  updateTask: async (id, input) => {
    await api.updateTask(id, input);
    await get().refreshTasks();
  },

  deleteTask: async (id) => {
    await api.deleteTask(id);
    set((state) => ({
      scheduledTasks: state.scheduledTasks.filter((t) => t.id !== id),
    }));
  },

  approveTask: async (id) => {
    await api.approveTask(id);
    await get().refreshTasks();
  },

  runTask: async (id) => {
    await api.runTask(id);
    setTimeout(() => void get().refreshTasks(), TASK_REFRESH_DELAY_MS);
  },
});
