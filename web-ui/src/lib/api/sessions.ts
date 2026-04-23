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

export type ReasoningEffortPatch = 'low' | 'medium' | 'high' | null;

export interface UpdateSessionPatch {
  title?: string;
  model?: string;
  /**
   * Tri-state effort override: omit to leave unchanged, `null` to clear, or a
   * specific level to set. The backend mirrors this semantics.
   */
  reasoningEffort?: ReasoningEffortPatch;
}

export async function updateSession(
  id: string,
  patch: UpdateSessionPatch,
): Promise<ApiSession> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(id)}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(patch),
  });
  if (!res.ok) throw new Error(`update session failed: ${res.status}`);
  return (await res.json()) as ApiSession;
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

export function pinSession(id: string): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/pin`, {});
}

export function unpinSession(id: string): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/unpin`, {});
}

export function moveSessionToFolder(id: string, folderId: string | null): Promise<void> {
  return apiPost<void>(`/sessions/${encodeURIComponent(id)}/move-to-folder`, { folderId });
}
