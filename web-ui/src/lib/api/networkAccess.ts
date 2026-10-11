import { apiGet } from './client';
import type { NetworkAccessLogsResponse } from '../types';

export function getNetworkAccessLogs(): Promise<NetworkAccessLogsResponse> {
  return apiGet<NetworkAccessLogsResponse>('/notifications/network-access');
}
