import type { StateCreator } from 'zustand';
import type { CreateTaskInput, ScheduledTask } from '@/types';
import * as api from '@/lib/api';
import type { AppState } from './index';
import { TASK_REFRESH_DELAY_MS } from './shared';

export interface ServicesSlice {
  mcpServers: api.McpServerInfo[];
  skills: api.SkillInfo[];
  scheduledTasks: ScheduledTask[];
  pushEnabled: boolean;
  remoteAccessStatus: api.RemoteAccessStatus | null;

  refreshMcpServers: () => Promise<void>;
  addMcpServer: (input: api.McpServerCreateRequest) => Promise<void>;
  removeMcpServer: (name: string) => Promise<void>;
  reconnectMcpServer: (name: string) => Promise<void>;

  refreshSkills: () => Promise<void>;
  installSkill: (repo: string) => Promise<void>;
  deleteSkill: (dirName: string) => Promise<void>;

  enablePush: () => Promise<void>;
  disablePush: () => Promise<void>;
  sendTestNotification: () => Promise<void>;
  refreshRemoteAccessStatus: () => Promise<void>;
  setRemoteAccessStatus: (status: api.RemoteAccessStatus) => void;
  startRemoteAccess: () => Promise<void>;
  stopRemoteAccess: () => Promise<void>;

  refreshTasks: () => Promise<void>;
  createTask: (input: CreateTaskInput) => Promise<ScheduledTask>;
  updateTask: (
    id: string,
    input: {
      name?: string;
      enabled?: boolean;
      cronExpression?: string;
      runAt?: string;
      scriptPath?: string;
      scriptInline?: string;
    },
  ) => Promise<void>;
  deleteTask: (id: string) => Promise<void>;
  approveTask: (id: string) => Promise<void>;
  runTask: (id: string) => Promise<void>;
}

export const createServicesSlice: StateCreator<
  AppState,
  [],
  [],
  ServicesSlice
> = (set, get) => ({
  mcpServers: [],
  skills: [],
  scheduledTasks: [],
  pushEnabled: false,
  remoteAccessStatus: null,

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
    const profileId = get().currentProfileId;
    if (!profileId) {
      set({ skills: [] });
      return;
    }
    const skills = await api.fetchSkills(profileId);
    set({ skills });
  },

  installSkill: async (repo) => {
    const profileId = get().currentProfileId;
    if (!profileId) {
      throw new Error('No active profile selected');
    }
    await api.installSkill(repo, profileId);
    await get().refreshSkills();
  },

  deleteSkill: async (dirName) => {
    const profileId = get().currentProfileId;
    if (!profileId) {
      throw new Error('No active profile selected');
    }
    await api.deleteSkill(dirName, profileId);
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

  refreshRemoteAccessStatus: async () => {
    const remoteAccessStatus = await api.fetchRemoteAccessStatus();
    set({ remoteAccessStatus });
  },

  setRemoteAccessStatus: (remoteAccessStatus) => {
    set({ remoteAccessStatus });
  },

  startRemoteAccess: async () => {
    await api.startRemoteAccess();
    await get().refreshRemoteAccessStatus();
  },

  stopRemoteAccess: async () => {
    await api.stopRemoteAccess();
    await get().refreshRemoteAccessStatus();
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
