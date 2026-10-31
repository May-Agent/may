import { create } from 'zustand';

/** The full-screen desktop viewer for a session's virtual display. */
interface ComputerViewState {
  viewerOpen: boolean;
  openViewer: () => void;
  closeViewer: () => void;
}

export const useComputerViewStore = create<ComputerViewState>((set) => ({
  viewerOpen: false,
  openViewer: () => set({ viewerOpen: true }),
  closeViewer: () => set({ viewerOpen: false }),
}));
