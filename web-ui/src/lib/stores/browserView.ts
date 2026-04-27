import { create } from 'zustand';
import { persist } from 'zustand/middleware';

interface BrowserViewState {
  /// User-controlled visibility of the live view side panel.
  open: boolean;
  /// Whether the panel has been auto-opened in this session by the
  /// runtime store reacting to a PeerConnected event. Stored so we
  /// only auto-open once per session and never override an explicit
  /// user dismiss.
  autoOpenedThisSession: boolean;

  setOpen: (open: boolean) => void;
  toggle: () => void;
  markAutoOpened: () => void;
}

export const useBrowserViewStore = create<BrowserViewState>()(
  persist(
    (set) => ({
      open: false,
      autoOpenedThisSession: false,

      setOpen: (open) => set({ open }),
      toggle: () => set((s) => ({ open: !s.open })),
      markAutoOpened: () => set({ autoOpenedThisSession: true }),
    }),
    {
      name: 'may-browser-view',
      partialize: (s) => ({ open: s.open }),
    },
  ),
);
