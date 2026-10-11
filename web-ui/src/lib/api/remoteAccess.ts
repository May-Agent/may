import { apiGet, apiPost } from './client';
import type { RemoteAccessStatusResponse } from '../types';

export function getRemoteAccessStatus(): Promise<RemoteAccessStatusResponse> {
  return apiGet<RemoteAccessStatusResponse>('/remote-access/status');
}

export function startRemoteAccess(): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>('/remote-access/start');
}

export function stopRemoteAccess(): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>('/remote-access/stop');
}
