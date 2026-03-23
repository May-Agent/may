import type { StateCreator } from 'zustand';
import type { ProfileInfo, ProfileSessions } from '@/types';
import * as api from '@/lib/api';
import type { AppState } from './index';
import {
  fetchAndApplyProfileSessions,
  isContainerTransientBlockedCode,
} from './shared';

export interface ProfilesSlice {
  profiles: ProfileInfo[];
  currentProfileId: string | null;
  profileSessions: ProfileSessions | null;
  profileSessionsLoading: boolean;
  profileAccessBlockedReason: string | null;
  profileAccessBlockedCode: string | null;
  backendUnavailableReason: string | null;
  containerPrepareStatuses: Record<string, api.ContainerPrepareStatus>;

  refreshProfiles: () => Promise<void>;
  markBackendUnavailable: (reason: string) => void;
  clearBackendUnavailable: () => void;
  setContainerPrepareStatuses: (statuses: api.ContainerPrepareStatus[]) => void;
  switchProfile: (profileId: string) => Promise<void>;
  refreshProfileSessions: (
    options?: api.FetchProfileSessionsOptions,
  ) => Promise<void>;
  createProfile: (input: {
    name: string;
    icon?: string;
    environment?: 'local' | 'container' | 'remote';
    containerImage?: string;
    persona?: string;
    preferences?: Record<string, unknown>;
    locationAware?: boolean;
  }) => Promise<ProfileInfo>;
  updateProfile: (id: string, input: {
    name?: string;
    icon?: string;
    persona?: string;
    preferences?: Record<string, unknown>;
    locationAware?: boolean;
  }) => Promise<void>;
  deleteProfile: (id: string) => Promise<void>;
  pinSession: (sessionId: string) => Promise<void>;
  unpinSession: (sessionId: string) => Promise<void>;
  archiveSession: (sessionId: string) => Promise<void>;
  unarchiveSession: (sessionId: string) => Promise<void>;
  moveSessionToFolder: (sessionId: string, folderId: string | null) => Promise<void>;
  createFolder: (name: string) => Promise<void>;
  updateFolder: (id: string, input: { name?: string; sortOrder?: number }) => Promise<void>;
  deleteFolder: (id: string) => Promise<void>;
}

export const createProfilesSlice: StateCreator<AppState, [], [], ProfilesSlice> = (set, get) => ({
  profiles: [],
  currentProfileId: null,
  profileSessions: null,
  profileSessionsLoading: false,
  profileAccessBlockedReason: null,
  profileAccessBlockedCode: null,
  backendUnavailableReason: null,
  containerPrepareStatuses: {},

  refreshProfiles: async () => {
    const profiles = await api.fetchProfiles();
    set({ profiles, backendUnavailableReason: null });
  },

  markBackendUnavailable: (reason) => {
    const normalized = reason.trim();
    if (!normalized) return;
    set({ backendUnavailableReason: normalized });
  },

  clearBackendUnavailable: () => {
    if (get().backendUnavailableReason !== null) {
      set({ backendUnavailableReason: null });
    }
  },

  setContainerPrepareStatuses: (statuses) => {
    const next: Record<string, api.ContainerPrepareStatus> = {};
    for (const status of statuses) {
      next[status.profileId] = status;
    }
    set((state) => {
      let nextBlockedReason = state.profileAccessBlockedReason;
      let nextBlockedCode = state.profileAccessBlockedCode;
      if (isContainerTransientBlockedCode(nextBlockedCode)) {
        const currentStatus = state.currentProfileId
          ? next[state.currentProfileId]
          : undefined;
        if (currentStatus && currentStatus.stage !== 'failed') {
          nextBlockedReason = null;
          nextBlockedCode = null;
        }
      }

      return {
        containerPrepareStatuses: next,
        profileAccessBlockedReason: nextBlockedReason,
        profileAccessBlockedCode: nextBlockedCode,
      };
    });
  },

  switchProfile: async (profileId) => {
    set({
      currentProfileId: profileId,
      currentSessionId: null,
      profileSessions: null,
      profileSessionsLoading: true,
      profileAccessBlockedReason: null,
      profileAccessBlockedCode: null,
    });
    void get().refreshSkills();
    await fetchAndApplyProfileSessions(profileId, get, set);
  },

  refreshProfileSessions: async (options) => {
    const profileId = get().currentProfileId;
    if (!profileId) return;
    set({ profileSessionsLoading: true });
    await fetchAndApplyProfileSessions(profileId, get, set, options);
  },

  createProfile: async (input) => {
    const profile = await api.createProfile(input);
    await get().refreshProfiles();
    return profile;
  },

  updateProfile: async (id, input) => {
    await api.updateProfile(id, input);
    await get().refreshProfiles();
    if (get().currentProfileId === id) {
      await get().refreshProfileSessions();
    }
  },

  deleteProfile: async (id) => {
    await api.deleteProfile(id);
    const profiles = get().profiles.filter((p) => p.id !== id);
    const nextProfileId = get().currentProfileId === id
      ? (profiles[0]?.id ?? null)
      : get().currentProfileId;
    set({
      profiles,
      currentProfileId: nextProfileId,
      profileSessions: null,
      profileSessionsLoading: false,
      profileAccessBlockedReason: null,
    });
    if (nextProfileId) {
      await get().switchProfile(nextProfileId);
    }
  },

  pinSession: async (sessionId) => {
    await api.pinSession(sessionId);
    await get().refreshProfileSessions();
  },

  unpinSession: async (sessionId) => {
    await api.unpinSession(sessionId);
    await get().refreshProfileSessions();
  },

  archiveSession: async (sessionId) => {
    await api.archiveSession(sessionId);
    await get().refreshProfileSessions();
  },

  unarchiveSession: async (sessionId) => {
    await api.unarchiveSession(sessionId);
    await get().refreshProfileSessions();
  },

  moveSessionToFolder: async (sessionId, folderId) => {
    await api.moveSessionToFolder(sessionId, folderId);
    await get().refreshProfileSessions();
  },

  createFolder: async (name) => {
    const profileId = get().currentProfileId;
    if (!profileId) return;
    await api.createFolder(profileId, name);
    await get().refreshProfileSessions();
  },

  updateFolder: async (id, input) => {
    await api.updateFolder(id, input);
    await get().refreshProfileSessions();
  },

  deleteFolder: async (id) => {
    await api.deleteFolder(id);
    await get().refreshProfileSessions();
  },
});
