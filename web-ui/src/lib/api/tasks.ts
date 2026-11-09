import { apiGet, apiPatch, apiPut } from './client';
import type {
  ApiSession,
  SessionTasksPayload,
  SessionPlanState,
} from '../types';

export function getSessionTasks(sessionId: string): Promise<SessionTasksPayload> {
  return apiGet<SessionTasksPayload>(`/sessions/${encodeURIComponent(sessionId)}/tasks`);
}

export function putSessionTasks(
  sessionId: string,
  payload: SessionTasksPayload,
): Promise<SessionTasksPayload> {
  return apiPut<SessionTasksPayload>(
    `/sessions/${encodeURIComponent(sessionId)}/tasks`,
    payload,
  );
}

export function putSessionPlanState(
  sessionId: string,
  planState: SessionPlanState,
): Promise<ApiSession> {
  return apiPatch<ApiSession>(`/sessions/${encodeURIComponent(sessionId)}`, { planState });
}
export function putSessionYolo(sessionId: string, yolo: boolean): Promise<ApiSession> {
  return apiPatch<ApiSession>(`/sessions/${encodeURIComponent(sessionId)}`, { yolo });
}

