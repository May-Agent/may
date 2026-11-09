import { apiPost } from './client';

export function respondPermission(id: string, approved: boolean): Promise<void> {
  return apiPost<void>(`/interactions/${encodeURIComponent(id)}`, { kind: 'permission', approved });
}

export function respondWorkspace(id: string, approved: boolean): Promise<void> {
  return apiPost<void>(`/interactions/${encodeURIComponent(id)}`, { kind: 'workspace', approved });
}

export type AskUserAnswer =
  | { kind: 'selected'; values: string[] }
  | { kind: 'custom'; text: string }
  /** Refuses to give the secret a secret question asks for. */
  | { kind: 'declined' };

export interface AskUserAnswerEntry {
  section: string;
  answer: AskUserAnswer;
}

export function respondAskUser(id: string, answers: AskUserAnswerEntry[]): Promise<void> {
  return apiPost<void>(`/interactions/${encodeURIComponent(id)}`, { kind: 'askUser', answers });
}
