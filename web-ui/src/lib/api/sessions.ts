import type { TranscriptPage } from '../transcript';
import type { ApiSession } from '../types';
import { apiDelete, apiGet, apiPatch, apiPost } from './client';

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

export function getTranscript(id: string): Promise<TranscriptPage> {
  return apiGet<TranscriptPage>(`/sessions/${id}/transcript`);
}

export function getSession(id: string): Promise<ApiSession> {
  return apiGet<ApiSession>(`/sessions/${encodeURIComponent(id)}`);
}

export function createSession(payload?: {
  title?: string;
  profileId?: string | null;
  model?: string;
  repo?: string;
}): Promise<ApiSession> {
  return apiPost<ApiSession>('/sessions', payload ?? {});
}

export type ReasoningEffortPatch = string | null;

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
    method: 'PATCH',
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
  return apiPatch<void>(`/sessions/${encodeURIComponent(id)}`, { archived: true });
}

export function unarchiveSession(id: string): Promise<void> {
  return apiPatch<void>(`/sessions/${encodeURIComponent(id)}`, { archived: false });
}

export function pinSession(id: string): Promise<void> {
  return apiPatch<void>(`/sessions/${encodeURIComponent(id)}`, { pinned: true });
}

export function unpinSession(id: string): Promise<void> {
  return apiPatch<void>(`/sessions/${encodeURIComponent(id)}`, { pinned: false });
}

export function moveSessionToFolder(id: string, folderId: string | null): Promise<void> {
  return apiPatch<void>(`/sessions/${encodeURIComponent(id)}`, { folderId });
}
