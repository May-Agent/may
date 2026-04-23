import { apiDelete, apiGet, apiPost, apiPut } from './client';
import type {
  CreateProfileRequest,
  ProfilePreferences,
  UpdateProfileRequest,
} from '../types';

export type HomeEnvironment =
  | { type: 'local' }
  | { type: 'container'; image?: string | null; container_id?: string | null };

export interface ProfileInfo {
  id: string;
  slug?: string | null;
  name: string;
  icon?: string | null;
  environment: HomeEnvironment;
  persona?: string | null;
  preferences?: ProfilePreferences | null;
  locationAware: boolean;
  pinnedLimit: number;
  sortOrder: number;
  /** SSH server names attached to this profile; reachable via the terminal
   *  tool with `mode: 'ssh'` + `ssh_server_id: <name>`. */
  remoteAttachments: string[];
  createdAt?: string;
  updatedAt?: string;
}

export function listProfiles(): Promise<ProfileInfo[]> {
  return apiGet<ProfileInfo[]>('/profiles');
}

export function createProfile(req: CreateProfileRequest): Promise<ProfileInfo> {
  return apiPost<ProfileInfo>('/profiles', req);
}

export function updateProfile(id: string, patch: UpdateProfileRequest): Promise<ProfileInfo> {
  return apiPut<ProfileInfo>(`/profiles/${encodeURIComponent(id)}`, patch);
}

export function deleteProfile(id: string): Promise<void> {
  return apiDelete<void>(`/profiles/${encodeURIComponent(id)}`);
}

export function setLastModel(model: string): Promise<void> {
  return apiPost<void>('/config/last-model', { model });
}

export function getProfileRemoteAttachments(profileId: string): Promise<string[]> {
  return apiGet<string[]>(`/profiles/${encodeURIComponent(profileId)}/remotes`);
}

export function setProfileRemoteAttachments(
  profileId: string,
  serverNames: string[],
): Promise<void> {
  return apiPut<void>(`/profiles/${encodeURIComponent(profileId)}/remotes`, {
    serverNames,
  });
}
