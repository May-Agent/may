import { apiPost } from './client';

export function respondPermission(id: string, approved: boolean): Promise<void> {
  return apiPost<void>('/permissions/respond', { id, approved });
}

export function respondWorkspace(id: string, approved: boolean): Promise<void> {
  return apiPost<void>('/workspace/respond', { id, approved });
}

export type AskUserAnswer =
  | { kind: 'selected'; values: string[] }
  | { kind: 'custom'; text: string }
  | { kind: 'skipped' };

export interface AskUserAnswerEntry {
  section: string;
  answer: AskUserAnswer;
}

export function respondAskUser(id: string, answers: AskUserAnswerEntry[]): Promise<void> {
  return apiPost<void>('/ask-user/respond', { id, answers });
}
