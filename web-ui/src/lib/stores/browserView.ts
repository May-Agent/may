import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type InspectorTab = 'details' | 'library' | 'computer';

interface BrowserViewState {
  /** The full-screen computer viewer. */
  viewerOpen: boolean;
  /** The inspector's selected tab: Details, Library or Computer. */
  inspectorTab: InspectorTab;
  /**
   * Whether the inspector already switched to the Computer tab on its own
   * this page load, when the agent first started using the browser. It
   * does so once, so it never fights a tab the user picked.
   */
  autoShownThisSession: boolean;

  openViewer: () => void;
  closeViewer: () => void;
  setInspectorTab: (tab: InspectorTab) => void;
  /** Show the Computer tab the first time the agent uses the browser. */
  revealComputerOnce: () => void;
}

export const useBrowserViewStore = create<BrowserViewState>()(
  persist(
    (set, get) => ({
      viewerOpen: false,
      inspectorTab: 'details',
      autoShownThisSession: false,

      openViewer: () => set({ viewerOpen: true }),
      closeViewer: () => set({ viewerOpen: false }),
      setInspectorTab: (inspectorTab) => set({ inspectorTab }),
      revealComputerOnce: () => {
        if (get().autoShownThisSession) return;
        set({ autoShownThisSession: true, inspectorTab: 'computer' });
      },
    }),
    {
      name: 'may-browser-view',
      partialize: (s) => ({ inspectorTab: s.inspectorTab }),
    },
  ),
);
