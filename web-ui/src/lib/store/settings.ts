import type { StateCreator } from 'zustand';
import * as api from '@/lib/api';
import { buildModelCatalog } from '@/lib/constants';
import type { AppState } from './index';

export interface SettingsSlice {
  settings: {
    font: string;
    theme: 'light' | 'dark' | 'system';
  };
  uiConfig: {
    sshServers: Array<{ name: string; target: string }>;
    maxIterations: number;
    fullConfig: api.UiFullConfig | null;
  };
  memoryContent: string;

  updateSettings: (settings: Partial<AppState['settings']>) => void;
  refreshRuntimeSettings: () => Promise<void>;
  refreshFullConfig: () => Promise<void>;
  refreshModelsFromProviders: () => Promise<void>;
  saveFullConfig: (config: api.UiFullConfig) => Promise<void>;
  setLastModel: (model: string) => Promise<void>;
  addSshServer: (name: string, target: string) => Promise<void>;
  removeSshServer: (name: string) => Promise<void>;
  refreshMemory: () => Promise<void>;
  saveMemory: (content: string) => Promise<void>;
}

export const createSettingsSlice: StateCreator<AppState, [], [], SettingsSlice> = (set, get) => ({
  settings: { font: 'Inter', theme: 'system' },
  uiConfig: {
    sshServers: [],
    maxIterations: 50,
    fullConfig: null,
  },
  memoryContent: '',

  updateSettings: (settings) => {
    set((state) => ({ settings: { ...state.settings, ...settings } }));
  },

  refreshRuntimeSettings: async () => {
    const config = await api.fetchUiConfig();
    set((state) => ({
      settings: {
        font: config.uiFont,
        theme: config.uiTheme,
      },
      uiConfig: {
        ...state.uiConfig,
        sshServers: config.sshServers,
        maxIterations: config.maxIterations,
      },
    }));
  },

  refreshFullConfig: async () => {
    const fullConfig = await api.fetchFullConfig();
    set((state) => ({ uiConfig: { ...state.uiConfig, fullConfig } }));
  },

  refreshModelsFromProviders: async () => {
    const payload = await api.fetchModels();
    set({ models: buildModelCatalog(payload.models, payload.details), modelsLoaded: true });
  },

  saveFullConfig: async (fullConfig) => {
    await api.saveFullConfig(fullConfig);
    set((state) => ({ uiConfig: { ...state.uiConfig, fullConfig } }));
  },

  setLastModel: async (model) => {
    await api.setLastModel(model);
    set((state) => ({
      uiConfig: {
        ...state.uiConfig,
        fullConfig: state.uiConfig.fullConfig
          ? { ...state.uiConfig.fullConfig, lastModel: model }
          : state.uiConfig.fullConfig,
      },
    }));
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
});
