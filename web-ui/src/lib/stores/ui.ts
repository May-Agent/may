import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type Theme = 'light' | 'dark' | 'system';

interface UiState {
  theme: Theme;
  sidebarCollapsed: boolean;
  commandOpen: boolean;

  setTheme: (t: Theme) => void;
  toggleSidebar: () => void;
  setCommandOpen: (open: boolean) => void;
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      theme: 'system',
      sidebarCollapsed: false,
      commandOpen: false,

      setTheme: (theme) => set({ theme }),
      toggleSidebar: () => set((p) => ({ sidebarCollapsed: !p.sidebarCollapsed })),
      setCommandOpen: (open) => set({ commandOpen: open }),
    }),
    {
      name: 'may-ui',
      partialize: (s) => ({ theme: s.theme, sidebarCollapsed: s.sidebarCollapsed }),
    },
  ),
);

export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  const effective =
    theme === 'system'
      ? window.matchMedia('(prefers-color-scheme: dark)').matches
        ? 'dark'
        : 'light'
      : theme;
  root.classList.toggle('dark', effective === 'dark');
}
