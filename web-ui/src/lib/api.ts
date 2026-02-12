import type { Session, Message, Attachment } from '@/types';

interface ApiAttachment {
  id: string;
  name: string;
  type: string;
  url: string;
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
}

export interface ModelDetailsItem {
  id: string;
  nickname: string;
  context_window?: number;
  kind?: 'llm' | 'image' | 'embedding' | 'reranker' | string;
  provider?: string;
  abilities: string[];
  reasoning_efforts?: string[];
  reasoning_effort_default?: string;
  adaptive_reasoning?: boolean;
  reasoning_budget_tokens_min?: number;
  reasoning_budget_tokens_max?: number;
  embedding_dimensions?: number;
  image_max_resolution?: string;
  reranker_max_documents?: number;
}

interface ApiSession {
  id: string;
  title: string;
  last_modified: string;
  preview: string;
  messages: ApiMessage[];
  model: string;
  reasoning_effort?: string;
}

interface ChatResponse {
  session_id: string;
  assistant: ApiMessage;
}

interface PermissionRequest {
  id: string;
  mode: 'sandboxed' | 'network' | 'unsafe';
  script: string;
}

export interface AskUserQuestion {
  section: string;
  question: string;
  option: string[];
  multi_select: boolean;
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
}

export interface ModelReasoningPolicy {
  enabled: boolean;
  forcedLow: boolean;
}

export interface UiFullConfig {
  providers: UiProviderConfig[];
  models: UiModelsConfig;
  preferences: UiPreferencesConfig;
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
  return { models: data.models, details: data.details ?? [] };
}

export async function pollPermissionRequest(): Promise<PermissionRequest | null> {
  const data = await request<{ request: PermissionRequest | null }>('/api/permissions/next');
  return data.request;
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
