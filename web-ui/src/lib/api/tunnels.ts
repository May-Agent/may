import { apiGet } from './client';

export interface TunnelInfo {
  id: string;
  target: string;
}

export function fetchTunnel(id: string): Promise<TunnelInfo> {
  return apiGet<TunnelInfo>(`/tunnels/${encodeURIComponent(id)}`);
}
