import { create } from 'zustand';
import { getConfig, type UiConfigResponse } from '../api/config';

interface ConfigState {
  config: UiConfigResponse | null;
  loading: boolean;
  error: string | null;

  setConfig: (config: UiConfigResponse) => void;
  refresh: () => Promise<void>;
}

export const useConfigStore = create<ConfigState>((set) => ({
  config: null,
  loading: false,
  error: null,

  setConfig(config) {
    set({ config, error: null });
  },

  async refresh() {
    set({ loading: true, error: null });
    try {
      const config = await getConfig();
      set({ config, loading: false });
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },
}));
