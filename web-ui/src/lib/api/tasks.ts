import { apiGet, apiPut } from './client';
import type {
  SessionTasksPayload,
  SessionPlanState,
  SessionPlanStateResponse,
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
): Promise<SessionPlanStateResponse> {
  return apiPut<SessionPlanStateResponse>(
    `/sessions/${encodeURIComponent(sessionId)}/tasks/plan-state`,
    { planState },
  );
}
