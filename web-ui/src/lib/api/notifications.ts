import { apiGet, apiPost } from './client';

export interface NotificationMailboxEntry {
  id: string;
  title?: string | null;
  body?: string | null;
  createdAt: string;
  status: 'pending' | 'delivered' | 'read' | string;
  sessionId?: string | null;
}

export interface NotificationMailboxResponse {
  items: NotificationMailboxEntry[];
}

export function getMailbox(): Promise<NotificationMailboxResponse> {
  return apiGet<NotificationMailboxResponse>('/notifications/mailbox');
}

export function fireNotificationCallback(id: string): Promise<void> {
  return apiPost<void>(`/notifications/${encodeURIComponent(id)}/callback`, {});
}
