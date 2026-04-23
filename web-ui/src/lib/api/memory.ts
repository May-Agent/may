import { apiGet, apiPost } from './client';
import type { MemoryPayload } from '../types';

export function getMemory(): Promise<MemoryPayload> {
  return apiGet<MemoryPayload>('/memory');
}

export function saveMemory(content: string): Promise<MemoryPayload> {
  return apiPost<MemoryPayload>('/memory', {
    content,
    explicitUserRequest: true,
  });
}
