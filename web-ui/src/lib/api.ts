import type { Session, Message, Attachment, Artifact } from '@/types';

interface ApiAttachment {
  id: string;
  name: string;
  type: string;
  url: string;
  upload_data_url?: string;
  text_content?: string;
}

interface ApiMessage {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: string;
  attachments?: ApiAttachment[];
  thinking?: string;
  is_streaming?: boolean;
  is_thinking?: boolean;
  tool_event?: {
    id: string;
    stage: 'start' | 'end';
    name?: string;
    arguments?: unknown;
    result?: unknown;
  };
  stats?: {
    provider_id?: string;
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
    context_window?: number;
    elapsed_ms?: number;
  };
}

interface ApiSession {
  id: string;
  title: string;
  last_modified: string;
  preview: string;
  messages: ApiMessage[];
  model: string;
  reasoning_effort?: 'low' | 'medium' | 'high';
}

interface ChatResponse {
  session_id: string;
  assistant: ApiMessage;
}

interface ApiArtifact {
  path: string;
  name: string;
  size: number;
  is_html: boolean;
  fingerprint?: string;
}

export interface PermissionRequest {
  id: string;
  mode: 'sandboxed' | 'network' | 'unsafe';
  script: string;
  session_id?: string;
}

export interface PermissionPollResult {
  request: PermissionRequest | null;
  state: 'waiting_permission' | null;
}

export interface SessionRuntimeStatus {
  state: 'idle' | 'streaming' | 'waiting_permission';
}

export interface AskUserQuestion {
  section: string;
  question: string;
  option: string[];
  multi_select: boolean;
}

export interface ArtifactStreamEvent {
  count: number;
  signature: string;
}

export type AskUserAnswerValue = string[] | string | null;

export interface AskUserAnswer {
  section: string;
  answer: AskUserAnswerValue;
}

export interface AskUserRequest {
  id: string;
  questions: AskUserQuestion[];
}

export interface UiConfig {
  ssh_servers: Array<{ name: string; target: string }>;
  max_iterations: number;
  ui_font: string;
  ui_theme: 'light' | 'dark' | 'system';
}

export type ProviderType =
  | 'anthropic'
  | 'openai'
  | 'google'
  | 'copilot'
  | 'openrouter'
  | 'deepseek'
  | 'ollama'
  | 'groq'
  | 'together'
  | 'mistral'
  | 'xai'
  | 'custom';

export interface UiProviderConfig {
  id: string;
  provider_type: ProviderType;
  api_key: string | null;
  base_url: string | null;
  display_name: string | null;
  enabled: boolean;
}

export interface ModelDetailsItem {
  id: string;
  nickname: string;
  context_window?: number;
  abilities: string[];
}

export interface ProviderModelsItem {
  provider_id: string;
  models: string[];
  details?: ModelDetailsItem[];
}

export interface RequestLogEntry {
  session_id: string;
  timestamp: string;
  model: string;
  provider_id: string;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  elapsed_ms: number;
  ok: boolean;
  error?: string | null;
}

export interface UiModelsConfig {
  advanced: string[];
  balanced: string[];
  fast: string[];
  image_generation: string[];
  model_providers: Record<string, string>;
}

export interface UiPreferencesConfig {
  load_balance_strategy: 'round_robin' | 'failover_only' | 'random';
  warn_on_model_change: boolean;
  show_token_usage: boolean;
}

export interface UiFullConfig {
  providers: UiProviderConfig[];
  models: UiModelsConfig;
  preferences: UiPreferencesConfig;
  appearance: {
    font: string;
    theme: 'light' | 'dark' | 'system';
  };
  last_model: string | null;
  max_iterations: number;
}

export interface McpServerInfo {
  name: string;
  source: string;
  status: string;
  is_connected: boolean;
  tool_count: number;
}

export interface SkillInfo {
  name: string;
  description: string;
  dir_name: string;
  source: string;
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

function mapAttachment(a: ApiAttachment): Attachment {
  return {
    id: a.id,
    name: a.name,
    type: (a.type as Attachment['type']) || 'file',
    url: a.url,
    textContent: a.text_content,
    textPreview: a.text_content?.slice(0, 200),
  };
}

function mapMessage(m: ApiMessage): Message {
  return {
    id: m.id,
    role: m.role,
    content: m.content,
    timestamp: m.timestamp,
    attachments: (m.attachments || []).map(mapAttachment),
    thinking: m.thinking,
    isStreaming: m.is_streaming,
    isThinking: m.is_thinking,
    bashExecutions: [],
    toolEvent: m.tool_event,
    stats: m.stats
      ? {
          providerId: m.stats.provider_id,
          promptTokens: m.stats.prompt_tokens,
          completionTokens: m.stats.completion_tokens,
          totalTokens: m.stats.total_tokens,
          contextWindow: m.stats.context_window,
          elapsedMs: m.stats.elapsed_ms,
        }
      : undefined,
  };
}

function mapSession(s: ApiSession): Session {
  return {
    id: s.id,
    title: s.title,
    lastModified: s.last_modified,
    preview: s.preview,
    messages: s.messages.map(mapMessage),
    model: s.model,
    reasoningEffort: s.reasoning_effort,
  };
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Request failed: ${res.status}`);
  }
  return (await res.json()) as T;
}

export async function fetchSessions(): Promise<Session[]> {
  const data = await request<ApiSession[]>('/api/sessions');
  return data.map(mapSession);
}

export async function createSession(model?: string): Promise<Session> {
  const data = await request<ApiSession>('/api/sessions', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ model }),
  });
  return mapSession(data);
}

export async function updateSession(id: string, payload: { title?: string; model?: string }): Promise<Session> {
  const data = await request<ApiSession>(`/api/sessions/${id}`, {
    method: 'PUT',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
  });
  return mapSession(data);
}

export async function removeSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/sessions/${id}`, { method: 'DELETE' });
}

export async function sendChat(input: {
  sessionId: string;
  content: string;
  attachments: Attachment[];
  signal?: AbortSignal;
}): Promise<Message> {
  const payload = {
    session_id: input.sessionId,
    content: input.content,
    attachments: input.attachments.map((a) => ({
      id: a.id,
      name: a.name,
      type: a.type,
      url: a.url,
      upload_data_url: a.uploadDataUrl,
      text_content: a.textContent,
    })),
  };

  const data = await request<ChatResponse>('/api/chat', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
    signal: input.signal,
  });

  return mapMessage(data.assistant);
}

export async function streamChat(input: {
  sessionId: string;
  content: string;
  attachments: Attachment[];
  onStart?: (assistantId: string) => void;
  onReasoning?: (chunk: string) => void;
  onText?: (chunk: string) => void;
  onToolStart?: (tool: unknown) => void;
  onToolEnd?: (tool: unknown) => void;
  onDone?: (assistant: Message) => void;
  onError?: (message: string) => void;
  signal?: AbortSignal;
}): Promise<void> {
  const payload = {
    session_id: input.sessionId,
    content: input.content,
    attachments: input.attachments.map((a) => ({
      id: a.id,
      name: a.name,
      type: a.type,
      url: a.url,
      upload_data_url: a.uploadDataUrl,
      text_content: a.textContent,
    })),
  };

  const res = await fetch('/api/chat/stream', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
    signal: input.signal,
  });
  if (!res.ok || !res.body) {
    throw new Error(await res.text());
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let eventName = '';
  let data = '';

  const flushEvent = () => {
    if (!data) return;
    const parsed = JSON.parse(data);
    switch (eventName) {
      case 'start':
        input.onStart?.(parsed.assistant_id);
        break;
      case 'reasoning':
        input.onReasoning?.(parsed.chunk || '');
        break;
      case 'text':
        input.onText?.(parsed.chunk || '');
        break;
      case 'tool_start':
        input.onToolStart?.(parsed);
        break;
      case 'tool_end':
        input.onToolEnd?.(parsed);
        break;
      case 'done':
        input.onDone?.(mapMessage(parsed.assistant));
        break;
      case 'error':
        input.onError?.(parsed.message || 'Unknown stream error');
        break;
      default:
        break;
    }
    eventName = '';
    data = '';
  };

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    let idx;
    while ((idx = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, idx).trimEnd();
      buffer = buffer.slice(idx + 1);

      if (!line) {
        flushEvent();
        continue;
      }
      if (line.startsWith('event:')) {
        eventName = line.slice(6).trim();
      } else if (line.startsWith('data:')) {
        data += line.slice(5).trim();
      }
    }
  }
}

export async function fetchModels(): Promise<{ models: string[]; details: ModelDetailsItem[] }> {
  const data = await request<{ models: string[]; details?: ModelDetailsItem[] }>('/api/models');
  return {
    models: data.models,
    details: data.details ?? [],
  };
}

export async function fetchModelsByProvider(): Promise<ProviderModelsItem[]> {
  const data = await request<{ providers: ProviderModelsItem[] }>('/api/models/by-provider');
  return data.providers;
}

export async function fetchRequestLogs(): Promise<RequestLogEntry[]> {
  const data = await request<{ logs: RequestLogEntry[] }>('/api/logs/requests');
  return data.logs;
}

export async function pollPermissionRequest(sessionId: string): Promise<PermissionPollResult> {
  const data = await request<PermissionPollResult>(`/api/permissions/next?session_id=${encodeURIComponent(sessionId)}`);
  return {
    request: data.request ?? null,
    state: data.state ?? null,
  };
}

export async function fetchSessionRuntime(sessionId: string): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>(`/api/sessions/${encodeURIComponent(sessionId)}/runtime`);
}

export async function fetchSessionArtifacts(sessionId: string): Promise<Artifact[]> {
  const data = await request<{ artifacts: ApiArtifact[] }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts`,
  );
  return data.artifacts.map((a) => ({
    path: a.path,
    name: a.name,
    size: a.size,
    isHtml: a.is_html,
    fingerprint: a.fingerprint,
  }));
}

export async function fetchArtifactPreview(
  sessionId: string,
  path: string,
): Promise<Response> {
  const res = await fetch(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/preview?path=${encodeURIComponent(path)}`,
    {
      cache: 'default',
    },
  );

  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Request failed: ${res.status}`);
  }

  return res;
}

export async function respondPermissionRequest(id: string, approved: boolean): Promise<void> {
  await request<{ ok: boolean }>('/api/permissions/respond', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ id, approved }),
  });
}

export async function pollAskUserRequest(): Promise<AskUserRequest | null> {
  const data = await request<{ request: AskUserRequest | null }>('/api/ask-user/next');
  return data.request;
}

export async function respondAskUserRequest(id: string, answers: AskUserAnswer[]): Promise<void> {
  await request<{ ok: boolean }>('/api/ask-user/respond', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ id, answers }),
  });
}

export async function fetchUiConfig(): Promise<UiConfig> {
  return request<UiConfig>('/api/config');
}

export async function fetchFullConfig(): Promise<UiFullConfig> {
  return request<UiFullConfig>('/api/config/full');
}

export async function saveFullConfig(config: UiFullConfig): Promise<void> {
  await request<{ ok: boolean }>('/api/config/full', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(config),
  });
}

export async function addSshServer(name: string, target: string): Promise<void> {
  await request<{ ok: boolean }>('/api/ssh', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ name, target }),
  });
}

export async function removeSshServer(name: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/ssh/${encodeURIComponent(name)}`, {
    method: 'DELETE',
  });
}

export async function fetchMcpServers(): Promise<McpServerInfo[]> {
  return request<McpServerInfo[]>('/api/mcp');
}

export async function addMcpServer(input: {
  name: string;
  command?: string;
  args?: string[];
  url?: string;
}): Promise<void> {
  await request<{ ok: boolean }>('/api/mcp', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function removeMcpServer(name: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/mcp/${encodeURIComponent(name)}`, {
    method: 'DELETE',
  });
}

export async function reconnectMcpServer(name: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/mcp/${encodeURIComponent(name)}/reconnect`, {
    method: 'POST',
  });
}

export async function fetchSkills(): Promise<SkillInfo[]> {
  return request<SkillInfo[]>('/api/skills');
}

export async function installSkill(repo: string): Promise<SkillInfo[]> {
  return request<SkillInfo[]>('/api/skills/install', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ repo }),
  });
}

export async function deleteSkill(dirName: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/skills/${encodeURIComponent(dirName)}`, {
    method: 'DELETE',
  });
}

export interface CopilotStartResponse {
  device_code: string;
  user_code: string;
  verification_uri: string;
  interval: number;
}

export interface CopilotPollResponse {
  status: 'pending' | 'success' | 'expired' | 'denied' | 'error';
  message?: string | null;
}

export async function fetchCopilotStatus(): Promise<{ authenticated: boolean }> {
  return request<{ authenticated: boolean }>('/api/copilot/status');
}

export async function startCopilotAuth(): Promise<CopilotStartResponse> {
  return request<CopilotStartResponse>('/api/copilot/start', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({}),
  });
}

export async function pollCopilotAuth(deviceCode: string): Promise<CopilotPollResponse> {
  return request<CopilotPollResponse>('/api/copilot/poll', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ device_code: deviceCode }),
  });
}

export async function logoutCopilot(): Promise<void> {
  await request<{ ok: boolean }>('/api/copilot/logout', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({}),
  });
}

export async function fetchMemory(): Promise<string> {
  const data = await request<{ content: string }>('/api/memory');
  return data.content;
}

export async function saveMemory(content: string): Promise<void> {
  await request<{ ok: boolean }>('/api/memory', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ content }),
  });
}

export async function deleteAllSessions(): Promise<void> {
  await request<{ ok: boolean }>('/api/sessions/delete-all', {
    method: 'POST',
  });
}

export function subscribeSessionArtifacts(
  sessionId: string,
  onArtifacts: (event: ArtifactStreamEvent) => void,
): () => void {
  const source = new EventSource(`/api/sessions/${encodeURIComponent(sessionId)}/artifacts/stream`);

  source.addEventListener('artifacts', (ev) => {
    try {
      const data = JSON.parse((ev as MessageEvent).data) as ArtifactStreamEvent;
      onArtifacts(data);
    } catch {
      // ignore malformed events
    }
  });

  return () => {
    source.close();
  };
}
