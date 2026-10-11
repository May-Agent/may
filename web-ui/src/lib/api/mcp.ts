import { apiDelete, apiGet, apiPost } from './client';

export interface McpServerInfo {
  name: string;
  kind: 'process' | 'url';
  command?: string | null;
  args?: string[] | null;
  env?: Record<string, string> | null;
  url?: string | null;
  status?: string | null;
}

export type AddMcpServerRequest =
  | { kind: 'process'; name: string; command: string; args?: string[]; env?: Record<string, string> }
  | { kind: 'url'; name: string; url: string };

export function listMcpServers(): Promise<McpServerInfo[]> {
  return apiGet<McpServerInfo[]>('/mcp');
}

export function addMcpServer(req: AddMcpServerRequest): Promise<void> {
  return apiPost<void>('/mcp', req);
}

export function removeMcpServer(name: string): Promise<void> {
  return apiDelete<void>(`/mcp/${encodeURIComponent(name)}`);
}

export function reconnectMcpServer(name: string): Promise<void> {
  return apiPost<void>(`/mcp/${encodeURIComponent(name)}/reconnect`, {});
}
