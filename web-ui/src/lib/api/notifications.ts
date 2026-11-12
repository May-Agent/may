import { apiGet } from './client';

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
