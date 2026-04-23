import { apiGet } from './client';
import type { RequestLogsResponse, SessionContextDebugResponse } from '../types';

export function getRequestLogs(): Promise<RequestLogsResponse> {
  return apiGet<RequestLogsResponse>('/logs/requests');
}

export function getSessionContextDebug(
  sessionId: string,
): Promise<SessionContextDebugResponse> {
  return apiGet<SessionContextDebugResponse>(
    `/sessions/${encodeURIComponent(sessionId)}/context-debug`,
  );
}
