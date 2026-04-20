import type { ApiSession, ApiMessage } from '../types';
import { apiDelete, apiGet, apiPost } from './client';

export interface SessionListResponse {
  items: Array<{
    id: string;
    title: string;
    lastModified: string;
    preview: string;
    profileId: string | null;
  }>;
}

export function listSessions(): Promise<SessionListResponse> {
  return apiGet<SessionListResponse>('/sessions');
}

export function getSession(id: string): Promise<ApiSession> {
  return apiGet<ApiSession>(`/sessions/${encodeURIComponent(id)}`);
}

export interface SessionMessagesResponse {
  items: ApiMessage[];
}

export function getSessionMessages(id: string): Promise<SessionMessagesResponse> {
  return apiGet<SessionMessagesResponse>(`/sessions/${encodeURIComponent(id)}/messages`);
}

export function createSession(payload?: { title?: string; profileId?: string | null }): Promise<ApiSession> {
  return apiPost<ApiSession>('/sessions', payload ?? {});
}

export function renameSession(id: string, title: string): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/rename`, { title });
}

export function deleteSession(id: string): Promise<void> {
  return apiDelete<void>(`/sessions/${encodeURIComponent(id)}`);
}

export function archiveSession(id: string): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/archive`, {});
}

export function unarchiveSession(id: string): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/unarchive`, {});
}
