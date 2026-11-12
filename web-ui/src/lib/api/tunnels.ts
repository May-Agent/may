import { apiGet } from './client';

export interface TunnelInfo {
  id: string;
  target: string;
}

/** A tunnel is named by a word of its session's ids, so it is addressed with its session. */
export function fetchTunnel(sessionId: string, id: string): Promise<TunnelInfo> {
  return apiGet<TunnelInfo>(
    `/tunnels/${encodeURIComponent(sessionId)}/${encodeURIComponent(id)}`,
  );
}
