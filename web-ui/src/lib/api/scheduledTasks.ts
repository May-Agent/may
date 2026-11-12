import { apiDelete, apiGet, apiPost, apiPut } from './client';
import type {
  CreateTaskRequest,
  ScheduledTaskResponse,
  TaskTriggerResponse,
  UpdateTaskRequest,
} from '../types';

export function listScheduledTasks(): Promise<ScheduledTaskResponse[]> {
  return apiGet<ScheduledTaskResponse[]>('/tasks');
}

export function getScheduledTask(id: string): Promise<ScheduledTaskResponse> {
  return apiGet<ScheduledTaskResponse>(`/tasks/${encodeURIComponent(id)}`);
}

export function createScheduledTask(req: CreateTaskRequest): Promise<ScheduledTaskResponse> {
  return apiPost<ScheduledTaskResponse>('/tasks', req);
}

export function updateScheduledTask(
  id: string,
  req: UpdateTaskRequest,
): Promise<ScheduledTaskResponse> {
  return apiPut<ScheduledTaskResponse>(`/tasks/${encodeURIComponent(id)}`, req);
}

export function deleteScheduledTask(id: string): Promise<void> {
  return apiDelete<void>(`/tasks/${encodeURIComponent(id)}`);
}

export function approveScheduledTask(id: string): Promise<{ ok: boolean }> {
  return apiPost<{ ok: boolean }>(`/tasks/${encodeURIComponent(id)}/approve`);
}

export function runScheduledTask(id: string): Promise<TaskTriggerResponse> {
  return apiPost<TaskTriggerResponse>(`/tasks/${encodeURIComponent(id)}/run`);
}
