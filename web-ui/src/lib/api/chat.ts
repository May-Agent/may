import type { ChatRequestPayload } from '../types';
import { ApiError } from './client';
import { markAuthRequired } from '../stores/authGate';

/** Start a turn; it streams through the session's event stream. */
export async function sendMessage(payload: ChatRequestPayload): Promise<void> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(payload.sessionId)}/turns`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!res.ok) {
    if (res.status === 401) markAuthRequired();
    const message = await res.text().catch(() => '');
    throw new ApiError(
      res.status === 409 ? 'bad_request' : 'internal_error',
      message || `send failed: ${res.status}`,
      res.status,
    );
  }
}

export async function enqueueMessage(payload: ChatRequestPayload): Promise<void> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(payload.sessionId)}/turns`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!res.ok) {
    if (res.status === 401) markAuthRequired();
    throw new ApiError('internal_error', 'enqueue failed', res.status);
  }
}

export async function stopSession(sessionId: string): Promise<void> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/turns/current`, {
    method: 'DELETE',
  });
  if (!res.ok) throw new ApiError('internal_error', 'stop failed', res.status);
}
