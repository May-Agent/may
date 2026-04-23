import type { BrowserStatusPayload } from '../types';
import { apiGet, apiPost } from './client';

export function getBrowserStatus(): Promise<BrowserStatusPayload> {
  return apiGet<BrowserStatusPayload>('/browser/status');
}

export function startBrowser(): Promise<BrowserStatusPayload> {
  return apiPost<BrowserStatusPayload>('/browser/start', {});
}

export function stopBrowser(): Promise<BrowserStatusPayload> {
  return apiPost<BrowserStatusPayload>('/browser/stop', {});
}

export function startTakeover(sessionId: string): Promise<{ ok: boolean; sessionId: string }> {
  return apiPost('/browser/takeover/start', { sessionId });
}

export function stopTakeover(sessionId: string): Promise<{ ok: boolean; sessionId: string }> {
  return apiPost('/browser/takeover/stop', { sessionId });
}
