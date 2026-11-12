import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type Theme = 'light' | 'dark' | 'system';

interface UiState {
  theme: Theme;
  /** Desktop sidebar hidden by the user. */
  sidebarCollapsed: boolean;
  /** Narrow screens: the sidebar shown as a sheet over the content. */
  sidebarSheetOpen: boolean;
  /** Wide screens: the session inspector pane shown beside the chat. */
  inspectorOpen: boolean;
  /** Narrow screens: the inspector shown as a sheet. */
  inspectorSheetOpen: boolean;
  commandOpen: boolean;

  setTheme: (t: Theme) => void;
  toggleSidebar: () => void;
  setSidebarSheetOpen: (open: boolean) => void;
  toggleInspector: () => void;
  setInspectorSheetOpen: (open: boolean) => void;
  setCommandOpen: (open: boolean) => void;
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      theme: 'system',
      sidebarCollapsed: false,
      sidebarSheetOpen: false,
      inspectorOpen: true,
      inspectorSheetOpen: false,
      commandOpen: false,

      setTheme: (theme) => set({ theme }),
      toggleSidebar: () => set((p) => ({ sidebarCollapsed: !p.sidebarCollapsed })),
      setSidebarSheetOpen: (open) => set({ sidebarSheetOpen: open }),
      toggleInspector: () => set((p) => ({ inspectorOpen: !p.inspectorOpen })),
      setInspectorSheetOpen: (open) => set({ inspectorSheetOpen: open }),
      setCommandOpen: (open) => set({ commandOpen: open }),
    }),
    {
      name: 'may-ui',
      partialize: (s) => ({
        theme: s.theme,
        sidebarCollapsed: s.sidebarCollapsed,
        inspectorOpen: s.inspectorOpen,
      }),
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
