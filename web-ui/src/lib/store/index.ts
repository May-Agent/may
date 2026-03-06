import { create } from 'zustand';
import { createSessionsSlice, type SessionsSlice } from './sessions';
import { createProfilesSlice, type ProfilesSlice } from './profiles';
import { createSettingsSlice, type SettingsSlice } from './settings';
import { createServicesSlice, type ServicesSlice } from './services';

export type AppState = SessionsSlice & ProfilesSlice & SettingsSlice & ServicesSlice;

export const useStore = create<AppState>((...args) => ({
  ...createSessionsSlice(...args),
  ...createProfilesSlice(...args),
  ...createSettingsSlice(...args),
  ...createServicesSlice(...args),
}));
