import { apiDelete, apiGet, apiPost, apiPut } from './client';
import type {
  CreateProfileRequest,
  MachineType,
  ProfilePreferences,
  UpdateProfileRequest,
} from '../types';

export type HomeEnvironment =
  | { type: 'local' }
  | { type: 'container'; container_id?: string | null };

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
   *  tool with `location: <name>`. */
  remoteAttachments: string[];
  /** Cloud provider accounts set up with `/cloud`. */
  cloud: { codespaces: { login: string } | null };
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

/** The machine types a cloud session of the profile on `repo` may start on. */
export async function getCloudCatalog(
  profileId: string,
  repo: string | null,
): Promise<MachineType[]> {
  const resp = await apiPost<{ machines: MachineType[] }>(
    `/profiles/${encodeURIComponent(profileId)}/cloud/codespaces/catalog`,
    { repo },
  );
  return resp.machines;
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
