import type { ProfilesResponse } from '../types';
import { apiGet, apiPost } from './client';

export function listProfiles(): Promise<ProfilesResponse> {
  return apiGet<ProfilesResponse>('/profiles');
}

export function setCurrentProfile(id: string): Promise<void> {
  return apiPost<void>(`/profiles/${encodeURIComponent(id)}/select`, {});
}
