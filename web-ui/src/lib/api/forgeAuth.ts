import { apiGet, apiPost } from './client';

export type ForgeAuthMode = 'oauth' | 'token' | 'cli';

export interface ForgeHostStatus {
  host: string;
  kind: string | null;
  configured: boolean;
  mode: ForgeAuthMode | null;
  client_id: string | null;
  authenticated: boolean;
}

export interface ForgeAuthStatusResponse {
  hosts: ForgeHostStatus[];
}

export interface ConfigureForgeAuthRequest {
  host: string;
  kind?: 'github' | 'gitlab';
  mode: ForgeAuthMode;
  client_id?: string;
  token?: string;
}

export interface ForgeDeviceAuthorization {
  device_code: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete: string | null;
  interval_secs: number;
  expires_in_secs: number;
}

export type ForgeOauthPollStatus = 'authorized' | 'pending' | 'slow_down';

export interface ForgeOauthPollResponse {
  status: ForgeOauthPollStatus;
  retry_after_secs: number;
}

export function getForgeAuthStatus(): Promise<ForgeAuthStatusResponse> {
  return apiGet<ForgeAuthStatusResponse>('/forge-auth');
}

export function configureForgeAuth(
  request: ConfigureForgeAuthRequest,
): Promise<ForgeHostStatus> {
  return apiPost<ForgeHostStatus>('/forge-auth/configure', request);
}

export function logoutForge(host: string): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>('/forge-auth/logout', { host });
}

export function startForgeOauth(request: {
  host: string;
  kind?: 'github' | 'gitlab';
  client_id?: string;
}): Promise<ForgeDeviceAuthorization> {
  return apiPost<ForgeDeviceAuthorization>('/forge-auth/oauth/start', request);
}

export function pollForgeOauth(
  host: string,
  deviceCode: string,
): Promise<ForgeOauthPollResponse> {
  return apiPost<ForgeOauthPollResponse>('/forge-auth/oauth/poll', {
    host,
    device_code: deviceCode,
  });
}
