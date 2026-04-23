import { create } from 'zustand';
import {
  createProfile,
  deleteProfile,
  listProfiles,
  updateProfile,
  type ProfileInfo,
} from '../api/profiles';
import type { CreateProfileRequest, UpdateProfileRequest } from '../types';
import type { BootstrapResponse } from '../api/bootstrap';

const LAST_PROFILE_STORAGE_KEY = 'may.lastProfileId';

export function readLastProfileId(): string | null {
  try {
    return typeof window === 'undefined' ? null : window.localStorage.getItem(LAST_PROFILE_STORAGE_KEY);
  } catch {
    return null;
  }
}

export function writeLastProfileId(id: string | null) {
  try {
    if (typeof window === 'undefined') return;
    if (id === null) window.localStorage.removeItem(LAST_PROFILE_STORAGE_KEY);
    else window.localStorage.setItem(LAST_PROFILE_STORAGE_KEY, id);
  } catch {
    // ignore
  }
}

interface ProfilesState {
  profiles: ProfileInfo[];
  defaultProfileId: string | null;
  loading: boolean;
  error: string | null;

  loadFromBootstrap: (resp: BootstrapResponse) => void;
  refresh: () => Promise<void>;
  create: (req: CreateProfileRequest) => Promise<ProfileInfo>;
  update: (id: string, patch: UpdateProfileRequest) => Promise<ProfileInfo>;
  remove: (id: string) => Promise<void>;
}

export const useProfilesStore = create<ProfilesState>((set) => ({
  profiles: [],
  defaultProfileId: null,
  loading: false,
  error: null,

  loadFromBootstrap(resp) {
    set({ profiles: resp.profiles, defaultProfileId: resp.defaultProfileId, loading: false, error: null });
  },

  async refresh() {
    set({ loading: true, error: null });
    try {
      const profiles = await listProfiles();
      set({ profiles, loading: false });
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : String(err) });
    }
  },

  async create(req) {
    const profile = await createProfile(req);
    set((prev) => ({ profiles: [...prev.profiles, profile] }));
    return profile;
  },

  async update(id, patch) {
    const updated = await updateProfile(id, patch);
    set((prev) => ({
      profiles: prev.profiles.map((p) => (p.id === id ? updated : p)),
    }));
    return updated;
  },

  async remove(id) {
    await deleteProfile(id);
    set((prev) => ({ profiles: prev.profiles.filter((p) => p.id !== id) }));
  },
}));

/**
 * Resolve the best profile id to land on when the URL has none. Preference
 * order: the preferred id (if still valid) → the server-provided default →
 * the first profile.
 */
export function resolveDefaultProfileId(
  profiles: ProfileInfo[],
  preferred: string | null,
  serverDefault: string | null,
): string | null {
  if (preferred && profiles.some((p) => p.id === preferred)) return preferred;
  if (serverDefault && profiles.some((p) => p.id === serverDefault)) return serverDefault;
  return profiles[0]?.id ?? null;
}
