import { apiGet } from './client';
import type { ApiErrorCode, ApiSession, ProfileSessions } from '../types';
import type { UiConfigResponse } from './config';
import type { ProfileInfo } from './profiles';

export interface BootstrapProfileSessionsError {
  code?: ApiErrorCode | null;
  message: string;
}

export interface SkillInfoDto {
  name: string;
  description?: string | null;
  dirName?: string | null;
  source?: string | null;
}

export interface BootstrapResponse {
  sessions: ApiSession[];
  profiles: ProfileInfo[];
  defaultProfileId: string;
  config: UiConfigResponse;
  defaultProfileSessions?: ProfileSessions | null;
  defaultProfileSessionsError?: BootstrapProfileSessionsError | null;
  defaultProfileSkills?: SkillInfoDto[];
}

export function getBootstrap(): Promise<BootstrapResponse> {
  return apiGet<BootstrapResponse>('/bootstrap');
}
