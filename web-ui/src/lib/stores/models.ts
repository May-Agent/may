import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { listModelsByProvider, type ProviderModelsGroup } from '../api/models';

interface ModelsState {
  groups: ProviderModelsGroup[] | null;
  fetchedAt: number | null;
  loading: boolean;
  error: string | null;

  refresh: () => Promise<void>;
}

let inflight: Promise<void> | null = null;

export const useModelsStore = create<ModelsState>()(
  persist(
    (set) => ({
      groups: null,
      fetchedAt: null,
      loading: false,
      error: null,

      refresh: async () => {
        if (inflight) return inflight;
        set({ loading: true });
        inflight = (async () => {
          try {
            const res = await listModelsByProvider();
            set({
              groups: res.providers,
              fetchedAt: Date.now(),
              loading: false,
              error: null,
            });
          } catch (err) {
            set({
              loading: false,
              error: err instanceof Error ? err.message : String(err),
            });
          } finally {
            inflight = null;
          }
        })();
        return inflight;
      },
    }),
    {
      name: 'may-models-cache',
      partialize: (s) => ({ groups: s.groups, fetchedAt: s.fetchedAt }),
    },
  ),
);
