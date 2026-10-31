import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type InspectorTab = 'details' | 'library' | 'browser';

interface BrowserViewState {
  /** The full-screen browser viewer. */
  viewerOpen: boolean;
  /** The inspector's selected tab: Details, Library or Browser. */
  inspectorTab: InspectorTab;
  /**
   * Whether the inspector already switched to the Browser tab on its own
   * this page load, when the agent first started using the browser. It
   * does so once, so it never fights a tab the user picked.
   */
  autoShownThisSession: boolean;

  openViewer: () => void;
  closeViewer: () => void;
  setInspectorTab: (tab: InspectorTab) => void;
  /** Show the Browser tab the first time the agent uses the browser. */
  revealBrowserOnce: () => void;
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
      revealBrowserOnce: () => {
        if (get().autoShownThisSession) return;
        set({ autoShownThisSession: true, inspectorTab: 'browser' });
      },
    }),
    {
      name: 'may-browser-view',
      partialize: (s) => ({ inspectorTab: s.inspectorTab }),
    },
  ),
);
