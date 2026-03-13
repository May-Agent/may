import type {
  Session,
  Message,
  Attachment,
  Artifact,
  TimelineSegment,
  BashExecution,
  ProfileInfo,
  ProfileSessions,
  FolderInfo,
  CreateTaskInput,
  ScheduledTask,
  VapidKeyInfo,
  PendingUserMessage,
  PendingUserMessageMode,
  SessionPlanState,
  SessionRuntimeState,
} from '@/types';

export interface PermissionRequest {
  id: string;
  mode: 'default' | 'unsafe' | 'ssh';
  script: string;
  sessionId?: string;
}

export interface SessionRuntimeStatus {
  state: SessionRuntimeState;
  planState: SessionPlanState;
  queuedCount: number;
  pendingImmediate?: PendingUserMessage | null;
  queuedMessages: PendingUserMessage[];
  waitingPermission?: PermissionRequest | null;
  waitingAskUser?: AskUserRequest | null;
}

export interface SessionRuntimeSnapshot {
  sessionId: string;
  runtime: SessionRuntimeStatus;
}

export interface AskUserQuestion {
  section: string;
  question: string;
  option: string[];
  multiSelect: boolean;
}

export interface ArtifactStreamEvent {
  count: number;
  signature: string;
}

export interface UiConfigEventPayload {
  uiFont?: string;
  uiTheme?: 'light' | 'dark' | 'system';
  sshServers?: Array<{ name: string; target: string }>;
  maxIterations?: number;
  fullConfig?: UiFullConfig;
}

export type ContainerPrepareStage =
  | 'checking'
  | 'building'
  | 'ready'
  | 'failed';

export interface ContainerPrepareStatus {
  profileId: string;
  stage: ContainerPrepareStage;
  summary: string;
  recentLogs: string[];
}

export interface ContainerPrepareEventPayload {
  statuses: ContainerPrepareStatus[];
}

export type RemoteAccessState = 'disabled' | 'starting' | 'running' | 'error';

export interface RemoteAccessStatus {
  state: RemoteAccessState;
  enabled: boolean;
  publicUrl?: string | null;
  lastError?: string | null;
  recentLogs: string[];
}

export interface UiEventsSubscription {
  sessionId?: string;
  profileId?: string;
  onOpen?: () => void;
  onConfig?: (payload: UiConfigEventPayload) => void;
  onContainerPrepare?: (payload: ContainerPrepareEventPayload) => void;
  onRemoteAccess?: (payload: { status: RemoteAccessStatus }) => void;
  onNotificationMailbox?: (payload: NotificationMailboxPayload) => void;
  onProfileSessions?: (payload: { profileSessions: ProfileSessions }) => void;
  onRequestLogs?: (payload: RequestLogsPayload) => void;
  onSessionRuntime?: (payload: { sessionId: string; runtime: SessionRuntimeStatus }) => void;
  onSessionRuntimes?: (payload: { sessions: SessionRuntimeSnapshot[] }) => void;
  onHeartbeat?: () => void;
  onError?: () => void;
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
  sshServers: Array<{ name: string; target: string }>;
  maxIterations: number;
  uiFont: string;
  uiTheme: 'light' | 'dark' | 'system';
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
  providerType: ProviderType;
  apiKey: string | null;
  baseUrl: string | null;
  displayName: string | null;
  enabled: boolean;
}

export interface ModelDetailsItem {
  id: string;
  nickname: string;
  contextWindow?: number;
  abilities: string[];
}

export interface ProviderModelsItem {
  providerId: string;
  models: string[];
  details?: ModelDetailsItem[];
}

export interface RequestLogEntry {
  sessionId: string;
  timestamp: string;
  model: string;
  providerId: string;
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
  contextWindow?: number;
  elapsedMs: number;
  ok: boolean;
  error?: string | null;
}

export interface RequestLogCacheSummary {
  assistantMessageCount: number;
  cacheHitMessageCount: number;
  promptTokensTotal: number;
  cacheReadTokensTotal: number;
  cacheWriteTokensTotal: number;
  cacheReadHitRate?: number;
}

export interface RequestLogsPayload {
  logs: RequestLogEntry[];
  cacheSummary: RequestLogCacheSummary;
}

export interface NotificationMailboxEntry {
  id: string;
  title: string;
  body: string;
  sessionId?: string | null;
  status: string;
  sentCount: number;
  lastError?: string | null;
  deliveredAt?: string | null;
  createdAt: string;
  updatedAt: string;
  hasCallback: boolean;
}

export interface NotificationMailboxPayload {
  items: NotificationMailboxEntry[];
}

export interface SessionTasksPayload {
  content: string;
  planState: SessionPlanState;
}

export interface ContextDebugMessage {
  role: string;
  content: string;
  attachments: string[];
  toolCallId?: string;
  toolCallNames: string[];
}

export type ContextWindowPhase =
  | "stable"
  | "reassembly_due"
  | "compression_due"
  | "handoff_due"
  | "handoff_active";

export interface SessionContextWindowMetrics {
  usageFraction: number;
  selectedModelContextWindow?: number;
  fastModelContextWindow?: number;
  effectiveContextWindow: number;
  systemBlockCount: number;
  reminderCount: number;
  recentMessageCount: number;
  recentSystemMessageCount: number;
  hasHandoff: boolean;
}

export interface SessionContextWindowSnapshot {
  phase: ContextWindowPhase;
  metrics: SessionContextWindowMetrics;
  assembledMessages: ContextDebugMessage[];
}

export interface SessionContextDebugSnapshot {
  sessionId: string;
  timestamp: string;
  model: string;
  providerId: string;
  prompt: string;
  promptAttachments: string[];
  assembledMessages: ContextDebugMessage[];
  windowSnapshot?: SessionContextWindowSnapshot;
}

export interface SessionContextDebugPayload {
  snapshot: SessionContextDebugSnapshot;
  rawJson: string;
}

function requireObject(value: unknown, field: string): Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`Invalid context debug payload: ${field} must be an object`);
  }
  return value as Record<string, unknown>;
}

function requireString(value: unknown, field: string): string {
  if (typeof value !== 'string') {
    throw new Error(`Invalid context debug payload: ${field} must be a string`);
  }
  return value;
}

function requireOptionalString(value: unknown, field: string): string | undefined {
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== 'string') {
    throw new Error(`Invalid context debug payload: ${field} must be a string when present`);
  }
  return value;
}

function requireStringArray(value: unknown, field: string): string[] {
  if (!Array.isArray(value)) {
    throw new Error(`Invalid context debug payload: ${field} must be an array`);
  }
  const result: string[] = [];
  for (let index = 0; index < value.length; index += 1) {
    const item = value[index];
    if (typeof item !== 'string') {
      throw new Error(`Invalid context debug payload: ${field}[${index}] must be a string`);
    }
    result.push(item);
  }
  return result;
}

function requireNumber(value: unknown, field: string): number {
  if (typeof value !== "number" || Number.isNaN(value)) {
    throw new Error(`Invalid context debug payload: ${field} must be a number`);
  }
  return value;
}

function requireBoolean(value: unknown, field: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`Invalid context debug payload: ${field} must be a boolean`);
  }
  return value;
}

function requireOptionalNumber(value: unknown, field: string): number | undefined {
  if (value === undefined || value === null) {
    return undefined;
  }
  return requireNumber(value, field);
}

function parseContextDebugMessage(value: unknown, index: number): ContextDebugMessage {
  const record = requireObject(value, `assembledMessages[${index}]`);
  return {
    role: requireString(record.role, `assembledMessages[${index}].role`),
    content: requireString(record.content, `assembledMessages[${index}].content`),
    attachments: requireStringArray(record.attachments, `assembledMessages[${index}].attachments`),
    toolCallId: requireOptionalString(record.toolCallId, `assembledMessages[${index}].toolCallId`),
    toolCallNames: requireStringArray(record.toolCallNames, `assembledMessages[${index}].toolCallNames`),
  };
}

function parseSessionContextWindowSnapshot(
  value: unknown,
): SessionContextWindowSnapshot {
  const record = requireObject(value, "windowSnapshot");
  const metrics = requireObject(record.metrics, "windowSnapshot.metrics");
  const assembledMessagesValue = record.assembledMessages;
  if (!Array.isArray(assembledMessagesValue)) {
    throw new Error("Invalid context debug payload: windowSnapshot.assembledMessages must be an array");
  }

  return {
    phase: requireString(record.phase, "windowSnapshot.phase") as ContextWindowPhase,
    metrics: {
      usageFraction: requireNumber(metrics.usageFraction, "windowSnapshot.metrics.usageFraction"),
      selectedModelContextWindow: requireOptionalNumber(metrics.selectedModelContextWindow, "windowSnapshot.metrics.selectedModelContextWindow"),
      fastModelContextWindow: requireOptionalNumber(metrics.fastModelContextWindow, "windowSnapshot.metrics.fastModelContextWindow"),
      effectiveContextWindow: requireNumber(metrics.effectiveContextWindow, "windowSnapshot.metrics.effectiveContextWindow"),
      systemBlockCount: requireNumber(metrics.systemBlockCount, "windowSnapshot.metrics.systemBlockCount"),
      reminderCount: requireNumber(metrics.reminderCount, "windowSnapshot.metrics.reminderCount"),
      recentMessageCount: requireNumber(metrics.recentMessageCount, "windowSnapshot.metrics.recentMessageCount"),
      recentSystemMessageCount: requireNumber(metrics.recentSystemMessageCount, "windowSnapshot.metrics.recentSystemMessageCount"),
      hasHandoff: requireBoolean(metrics.hasHandoff, "windowSnapshot.metrics.hasHandoff"),
    },
    assembledMessages: assembledMessagesValue.map((item, index) =>
      parseContextDebugMessage(item, index),
    ),
  };
}

function parseSessionContextDebugSnapshot(value: unknown): SessionContextDebugSnapshot {
  const record = requireObject(value, 'root');
  const assembledMessagesValue = record.assembledMessages;
  if (!Array.isArray(assembledMessagesValue)) {
    throw new Error('Invalid context debug payload: assembledMessages must be an array');
  }

  return {
    sessionId: requireString(record.sessionId, 'sessionId'),
    timestamp: requireString(record.timestamp, 'timestamp'),
    model: requireString(record.model, 'model'),
    providerId: requireString(record.providerId, 'providerId'),
    prompt: requireString(record.prompt, 'prompt'),
    promptAttachments: requireStringArray(record.promptAttachments, 'promptAttachments'),
    assembledMessages: assembledMessagesValue.map((item, index) =>
      parseContextDebugMessage(item, index),
    ),
    windowSnapshot:
      record.windowSnapshot === undefined || record.windowSnapshot === null
        ? undefined
        : parseSessionContextWindowSnapshot(record.windowSnapshot),
  };
}

export interface UiModelsConfig {
  advanced: string[];
  balanced: string[];
  fast: string[];
  imageGeneration: string[];
  modelProviders: Record<string, string>;
}

export interface UiPreferencesConfig {
  loadBalanceStrategy: 'round_robin' | 'failover_only' | 'random';
  warnOnModelChange: boolean;
  showTokenUsage: boolean;
  strictModelMatching: boolean;
  networkPolicyMode: 'disabled' | 'restricted' | 'allow_list' | 'deny_list';
  networkPolicyHosts: string[];
}

export interface UiPlaywrightConfig {
  enabled: boolean;
  browser: 'local_chrome' | 'chromium';
  port: number;
  screencastQuality: number;
  screencastMaxWidth: number;
}

export interface UiRemoteAccessConfig {
  enabled: boolean;
  cloudflaredBinary: string;
  tunnelToken?: string | null;
  publicUrl?: string | null;
  allowLocalEnvironment: boolean;
}

export interface UiFullConfig {
  providers: UiProviderConfig[];
  models: UiModelsConfig;
  preferences: UiPreferencesConfig;
  appearance: {
    font: string;
    theme: 'light' | 'dark' | 'system';
  };
  lastModel: string | null;
  maxIterations: number;
  playwright: UiPlaywrightConfig;
  remoteAccess: UiRemoteAccessConfig;
}

export interface McpServerInfo {
  name: string;
  source: string;
  status: string;
  isConnected: boolean;
  toolCount: number;
}

export interface SkillInfo {
  name: string;
  description: string;
  dirName: string;
  source: string;
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

// ── Typed API Error ──────────────────────────────────────

/**
 * Typed API error carrying a machine-readable `code` when the backend provides one.
 *
 * Error codes from the backend (e.g. `container_preparing`, `container_build_failed`)
 * allow the frontend to distinguish error categories without string-matching messages.
 * Network-level errors (connection refused, proxy 502/503/504) are assigned the
 * synthetic code `backend_unavailable`.
 */
export class ApiError extends Error {
  readonly code: string | null;
  readonly statusCode: number;

  constructor(message: string, statusCode: number, code: string | null = null) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.statusCode = statusCode;
  }
}

// ── Enrichment helpers ───────────────────────────────────
// The backend sends camelCase JSON matching our types directly.
// These helpers add client-side computed fields.

function enrichAttachment(a: Attachment): Attachment {
  return {
    ...a,
    textPreview: a.textContent?.slice(0, 200),
  };
}

function buildTimelineSegments(m: Message): TimelineSegment[] {
  const segments: TimelineSegment[] = [];
  const anchorMs = Date.parse(m.timestamp);
  const startedAt = Number.isFinite(anchorMs) ? anchorMs : undefined;

  if (typeof m.thinking === 'string' && m.thinking.trim().length > 0) {
    segments.push({
      id: `${m.id}-thinking`,
      kind: 'thinking',
      offset: 0,
      content: m.thinking,
      isRunning: !!m.isThinking,
      startedAt,
      endedAt: m.isThinking ? undefined : startedAt,
    });
  }

  if (Array.isArray(m.bashExecutions) && m.bashExecutions.length > 0) {
    segments.push({
      id: `${m.id}-bash`,
      kind: 'bash',
      offset: 0,
      commands: m.bashExecutions,
      startedAt,
      endedAt: m.bashExecutions.some((x: BashExecution) => x.status === 'running')
        ? undefined
        : startedAt,
    });
  }

  return segments;
}

function enrichMessage(m: Message): Message {
  return {
    ...m,
    attachments: (m.attachments || []).map(enrichAttachment),
    timelineSegments: buildTimelineSegments(m),
  };
}

function enrichSession(s: Session): Session {
  return {
    ...s,
    messages: s.messages.map(enrichMessage),
  };
}

// ── SSE Helpers ──────────────────────────────────────────

function buildSSEListener<T>(
  eventName: string,
  handler: ((data: T) => void) | undefined,
  onError: (() => void) | undefined,
): (ev: MessageEvent) => void {
  return (ev) => {
    if (!handler) return;
    try {
      const data = JSON.parse(ev.data) as T;
      handler(data);
    } catch (error) {
      console.error(
        `invalid ${eventName} event payload`,
        error instanceof Error ? error.message : String(error),
      );
      onError?.();
    }
  };
}

interface ReconnectingEventSourceHandlers {
  onOpen?: () => void;
  onError?: () => void;
  listeners: Record<string, (ev: MessageEvent) => void>;
}

function createReconnectingEventSource(
  url: string,
  handlers: ReconnectingEventSourceHandlers,
): () => void {
  let source: EventSource | null = null;
  let reconnectTimer: number | null = null;
  let reconnectAttempt = 0;
  let closed = false;

  const clearReconnectTimer = () => {
    if (reconnectTimer != null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  };

  const scheduleReconnect = () => {
    if (closed || reconnectTimer != null) return;
    const delayMs = Math.min(1000 * 2 ** reconnectAttempt, 10000);
    reconnectAttempt += 1;
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delayMs);
  };

  const connect = () => {
    if (closed) return;
    const next = new EventSource(url);
    source = next;

    next.onopen = () => {
      reconnectAttempt = 0;
      clearReconnectTimer();
      handlers.onOpen?.();
    };

    for (const [name, handler] of Object.entries(handlers.listeners)) {
      next.addEventListener(name, handler);
    }

    next.onerror = () => {
      handlers.onError?.();
      next.close();
      if (source === next) {
        source = null;
      }
      scheduleReconnect();
    };
  };

  connect();

  return () => {
    closed = true;
    clearReconnectTimer();
    if (source) {
      source.close();
      source = null;
    }
  };
}

// ── HTTP Helpers ─────────────────────────────────────────

function buildRequestError(url: string, res: Response, text: string): ApiError {
  let decodedMessage: string | null = null;
  let errorCode: string | null = null;

  if (text) {
    try {
      const parsed = JSON.parse(text) as { error?: unknown; message?: unknown; code?: unknown };
      // Machine-readable error code from the backend (Phase 4 typed errors).
      if (typeof parsed.code === 'string' && parsed.code.trim().length > 0) {
        errorCode = parsed.code.trim();
      }
      if (typeof parsed.error === 'string' && parsed.error.trim().length > 0) {
        decodedMessage = parsed.error.trim();
      }
      if (
        !decodedMessage &&
        typeof parsed.message === 'string' &&
        parsed.message.trim().length > 0
      ) {
        decodedMessage = parsed.message.trim();
      }
    } catch {
      decodedMessage = null;
    }
  }

  // Detect proxy-level backend unavailability by HTTP status alone.
  // When the backend is down, the Vite dev proxy (or any reverse proxy) returns
  // 502/503/504 — no string matching needed.
  const rawMessage = (decodedMessage ?? text ?? '').trim();
  const normalized = rawMessage.toLowerCase();
  const looksLikeHtmlBody =
    normalized.startsWith('<!doctype html') || normalized.startsWith('<html');
  const isProxyBackendUnavailable =
    url.startsWith('/api/') &&
    (res.status === 502 ||
      res.status === 503 ||
      res.status === 504 ||
      (res.status >= 500 && rawMessage.length === 0) ||
      looksLikeHtmlBody);

  if (isProxyBackendUnavailable) {
    const compact =
      rawMessage.length > 0
        ? rawMessage.replace(/\s+/g, ' ').slice(0, 220)
        : `request ${url} failed with status ${res.status}`;
    return new ApiError(`Backend unavailable: ${compact}`, res.status, 'backend_unavailable');
  }

  const message = decodedMessage || text || `Request failed: ${res.status}`;
  return new ApiError(message, res.status, errorCode);
}

async function requestResponse(url: string, init?: RequestInit): Promise<Response> {
  let res: Response;
  try {
    res = await fetch(url, init);
  } catch (fetchError) {
    // Network-level error: the request never reached the server (connection refused, DNS, etc.).
    const detail = fetchError instanceof Error ? fetchError.message : String(fetchError);
    throw new ApiError(`Backend unavailable: ${detail}`, 0, 'backend_unavailable');
  }
  if (!res.ok) {
    const text = await res.text();
    throw buildRequestError(url, res, text);
  }
  return res;
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await requestResponse(url, init);
  return (await res.json()) as T;
}

// ── Session API ──────────────────────────────────────────

export async function fetchSessions(): Promise<Session[]> {
  const data = await request<Session[]>('/api/sessions');
  return data.map(enrichSession);
}

export async function fetchSession(id: string): Promise<Session> {
  const data = await request<Session>(`/api/sessions/${id}`);
  return enrichSession(data);
}

export async function createSession(model?: string, profileId?: string): Promise<Session> {
  const data = await request<Session>('/api/sessions', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ model, profileId }),
  });
  return enrichSession(data);
}

export async function updateSession(id: string, payload: { title?: string; model?: string }): Promise<Session> {
  const data = await request<Session>(`/api/sessions/${id}`, {
    method: 'PUT',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
  });
  return enrichSession(data);
}

export async function removeSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/sessions/${id}`, { method: 'DELETE' });
}

// ── Chat API ─────────────────────────────────────────────

export async function sendChat(input: {
  sessionId: string;
  content: string;
  attachments: Attachment[];
  hiddenContextXml?: string;
  signal?: AbortSignal;
}): Promise<Message> {
  const payload = {
    sessionId: input.sessionId,
    content: input.content,
    hiddenContextXml: input.hiddenContextXml,
    attachments: input.attachments.map((a) => ({
      id: a.id,
      name: a.name,
      type: a.type,
      url: a.url,
      uploadDataUrl: a.uploadDataUrl,
      textContent: a.textContent,
    })),
  };

  const data = await request<{ sessionId: string; assistant: Message }>('/api/chat', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
    signal: input.signal,
  });

  return enrichMessage(data.assistant);
}

export async function streamChat(input: {
  sessionId: string;
  content: string;
  attachments: Attachment[];
  hiddenContextXml?: string;
  onStart?: (assistantId: string) => void;
  onReasoning?: (chunk: string) => void;
  onText?: (chunk: string) => void;
  onToolStart?: (tool: unknown) => void;
  onToolEnd?: (tool: unknown) => void;
  onTodoUpdate?: (event: { items: Array<{ text: string; checked: boolean }>; total: number; completed: number }) => void;
  onDone?: (assistant: Message) => void;
  onError?: (message: string) => void;
  signal?: AbortSignal;
}): Promise<void> {
  const payload = {
    sessionId: input.sessionId,
    content: input.content,
    hiddenContextXml: input.hiddenContextXml,
    attachments: input.attachments.map((a) => ({
      id: a.id,
      name: a.name,
      type: a.type,
      url: a.url,
      uploadDataUrl: a.uploadDataUrl,
      textContent: a.textContent,
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
    const requireString = (value: unknown, field: string): string => {
      if (typeof value !== 'string') {
        throw new Error(`Invalid SSE payload: expected string field '${field}'`);
      }
      return value;
    };

    switch (eventName) {
      case 'start':
        input.onStart?.(requireString(parsed.assistantId, 'assistantId'));
        break;
      case 'reasoning':
        input.onReasoning?.(requireString(parsed.chunk, 'chunk'));
        break;
      case 'text':
        input.onText?.(requireString(parsed.chunk, 'chunk'));
        break;
      case 'tool_start':
        input.onToolStart?.(parsed);
        break;
      case 'tool_end':
        input.onToolEnd?.(parsed);
        break;
      case 'todo_update':
        input.onTodoUpdate?.(parsed);
        break;
      case 'done':
        if (!parsed.assistant || typeof parsed.assistant !== 'object') {
          throw new Error("Invalid SSE payload: missing 'assistant' in done event");
        }
        input.onDone?.(enrichMessage(parsed.assistant));
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

  // Process a trailing event when stream ends without a final blank line.
  if (data) {
    flushEvent();
  }
}

export async function enqueueChat(input: {
  sessionId: string;
  content: string;
  attachments: Attachment[];
  hiddenContextXml?: string;
  deliveryMode?: PendingUserMessageMode;
  signal?: AbortSignal;
}): Promise<SessionRuntimeStatus> {
  const payload = {
    sessionId: input.sessionId,
    content: input.content,
    hiddenContextXml: input.hiddenContextXml,
    deliveryMode: input.deliveryMode ?? 'steer',
    attachments: input.attachments.map((a) => ({
      id: a.id,
      name: a.name,
      type: a.type,
      url: a.url,
      uploadDataUrl: a.uploadDataUrl,
      textContent: a.textContent,
    })),
  };

  return request<SessionRuntimeStatus>('/api/chat/enqueue', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(payload),
    signal: input.signal,
  });
}

// ── Models API ───────────────────────────────────────────

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

export async function fetchRequestLogs(): Promise<RequestLogsPayload> {
  return request<RequestLogsPayload>('/api/logs/requests');
}

export async function fetchNotificationMailbox(): Promise<NotificationMailboxPayload> {
  return request<NotificationMailboxPayload>('/api/notifications/mailbox');
}

export async function fetchSessionRuntime(sessionId: string): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>(`/api/sessions/${encodeURIComponent(sessionId)}/runtime`);
}

export async function stopSessionRun(sessionId: string): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>(`/api/sessions/${encodeURIComponent(sessionId)}/stop`, {
    method: 'POST',
    headers: JSON_HEADERS,
  });
}

export async function removePendingSessionMessage(input: {
  sessionId: string;
  kind: 'pending_immediate' | 'queued';
  index?: number;
}): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>(
    `/api/sessions/${encodeURIComponent(input.sessionId)}/runtime/pending`,
    {
      method: 'DELETE',
      headers: JSON_HEADERS,
      body: JSON.stringify({
        kind: input.kind,
        index: input.index,
      }),
    },
  );
}

export async function fetchSessionContextDebug(
  sessionId: string,
): Promise<SessionContextDebugPayload> {
  const response = await requestResponse(
    `/api/sessions/${encodeURIComponent(sessionId)}/context-debug`,
    {
      cache: 'no-store',
    },
  );
  const rawJson = await response.text();
  if (rawJson.trim().length === 0) {
    throw new Error('Invalid context debug payload: empty response body');
  }
  let payload: unknown;
  try {
    payload = JSON.parse(rawJson);
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    throw new Error(`Invalid context debug payload: non-JSON response (${reason})`);
  }
  return {
    snapshot: parseSessionContextDebugSnapshot(payload),
    rawJson,
  };
}

// ── Artifacts API ────────────────────────────────────────

export async function fetchSessionArtifacts(sessionId: string): Promise<Artifact[]> {
  const data = await request<{ artifacts: Artifact[] }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts`,
  );
  return data.artifacts;
}

export async function fetchArtifactPreview(
  sessionId: string,
  path: string,
): Promise<Response> {
  const trimmed = path.trim();
  if (!trimmed) {
    throw new Error('artifact path is required');
  }

  const res = await fetch(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/preview?path=${encodeURIComponent(trimmed)}`,
    {
      cache: 'no-store',
    },
  );

  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `Request failed: ${res.status}`);
  }

  return res;
}

export async function prepareSlidevPreview(
  sessionId: string,
  path: string,
): Promise<string> {
  const trimmed = path.trim();
  if (!trimmed) {
    throw new Error('artifact path is required');
  }
  const data = await request<{ previewUrl: string }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/slidev/preview?path=${encodeURIComponent(trimmed)}`,
    {
      cache: 'no-store',
    },
  );
  const previewUrl = data.previewUrl?.trim();
  if (!previewUrl) {
    throw new Error('slidev preview url is empty');
  }
  return previewUrl;
}

export function buildSessionOutputPreviewUrl(sessionId: string, path: string): string {
  const safeSessionId = sessionId.trim();
  const safePath = path.trim();
  if (!safeSessionId) {
    throw new Error('session id is required');
  }
  if (!safePath) {
    throw new Error('output path is required');
  }
  return `/api/sessions/${encodeURIComponent(safeSessionId)}/outputs/preview?path=${encodeURIComponent(safePath)}`;
}

export async function mapContainerLocalArtifactUrl(
  sessionId: string,
  url: string,
): Promise<string> {
  const trimmed = url.trim();
  if (!trimmed) {
    throw new Error('local url is required');
  }
  const data = await request<{ mappedUrl: string }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/local-url-map`,
    {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ url: trimmed }),
    },
  );
  return data.mappedUrl;
}

// ── Permission & Ask-User API ────────────────────────────

export async function respondPermissionRequest(
  id: string,
  approved: boolean,
): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>('/api/permissions/respond', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ id, approved }),
  });
}

export async function respondAskUserRequest(
  id: string,
  answers: AskUserAnswer[],
): Promise<SessionRuntimeStatus> {
  return request<SessionRuntimeStatus>('/api/ask-user/respond', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ id, answers }),
  });
}

// ── Config API ───────────────────────────────────────────

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

export async function fetchRemoteAccessStatus(): Promise<RemoteAccessStatus> {
  return request<RemoteAccessStatus>('/api/remote-access/status');
}

export async function startRemoteAccess(): Promise<void> {
  await request<{ ok: boolean }>('/api/remote-access/start', {
    method: 'POST',
  });
}

export async function stopRemoteAccess(): Promise<void> {
  await request<{ ok: boolean }>('/api/remote-access/stop', {
    method: 'POST',
  });
}

export async function setLastModel(model: string): Promise<void> {
  await request<{ ok: boolean }>('/api/config/last-model', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ model }),
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

// ── MCP API ──────────────────────────────────────────────

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

// ── Skills API ───────────────────────────────────────────

export async function fetchSkills(profileId: string): Promise<SkillInfo[]> {
  return request<SkillInfo[]>(
    `/api/skills?profileId=${encodeURIComponent(profileId)}`,
  );
}

export async function installSkill(repo: string, profileId: string): Promise<SkillInfo[]> {
  return request<SkillInfo[]>(
    `/api/skills/install?profileId=${encodeURIComponent(profileId)}`,
    {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ repo }),
    },
  );
}

export async function deleteSkill(dirName: string, profileId: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/skills/${encodeURIComponent(dirName)}?profileId=${encodeURIComponent(profileId)}`,
    {
      method: 'DELETE',
    },
  );
}

// ── Copilot Auth API ─────────────────────────────────────

export interface CopilotStartResponse {
  deviceCode: string;
  userCode: string;
  verificationUri: string;
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
    body: JSON.stringify({ deviceCode }),
  });
}

export async function logoutCopilot(): Promise<void> {
  await request<{ ok: boolean }>('/api/copilot/logout', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({}),
  });
}

// ── Memory API ───────────────────────────────────────────

export async function fetchMemory(): Promise<string> {
  const data = await request<{ content: string }>('/api/memory');
  return data.content;
}

export async function saveMemory(content: string): Promise<void> {
  await request<{ ok: boolean }>('/api/memory', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ content, explicitUserRequest: true }),
  });
}

export async function deleteAllSessions(): Promise<void> {
  await request<{ ok: boolean }>('/api/sessions/delete-all', {
    method: 'POST',
  });
}

// ── SSE Subscriptions ────────────────────────────────────

export interface SessionArtifactsSubscription {
  onArtifacts: (event: ArtifactStreamEvent) => void;
  onOpen?: () => void;
  onHeartbeat?: () => void;
  onError?: () => void;
}

export function subscribeSessionArtifacts(
  sessionId: string,
  options: SessionArtifactsSubscription,
): () => void {
  const streamUrl = `/api/sessions/${encodeURIComponent(sessionId)}/artifacts/stream`;

  return createReconnectingEventSource(streamUrl, {
    onOpen: options.onOpen,
    onError: options.onError,
    listeners: {
      artifacts: (ev) => {
        try {
          const data = JSON.parse(ev.data) as ArtifactStreamEvent;
          options.onArtifacts(data);
        } catch (error) {
          console.error(
            'invalid artifacts stream payload',
            error instanceof Error ? error.message : String(error),
          );
          options.onError?.();
        }
      },
      heartbeat: () => {
        options.onHeartbeat?.();
      },
    },
  });
}

export function subscribeUiEvents(options: UiEventsSubscription): () => void {
  const params = new URLSearchParams();
  if (options.sessionId && options.sessionId.trim()) {
    params.set('sessionId', options.sessionId.trim());
  }
  if (options.profileId && options.profileId.trim()) {
    params.set('profileId', options.profileId.trim());
  }

  const query = params.toString();
  const url = query ? `/api/events?${query}` : '/api/events';

  return createReconnectingEventSource(url, {
    onOpen: options.onOpen,
    onError: options.onError,
    listeners: {
      config: buildSSEListener<UiConfigEventPayload>(
        'config',
        options.onConfig,
        options.onError,
      ),
      request_logs: buildSSEListener<RequestLogsPayload>(
        'request_logs',
        options.onRequestLogs,
        options.onError,
      ),
      notification_mailbox: buildSSEListener<NotificationMailboxPayload>(
        'notification_mailbox',
        options.onNotificationMailbox,
        options.onError,
      ),
      container_prepare: buildSSEListener<ContainerPrepareEventPayload>(
        'container_prepare',
        options.onContainerPrepare
          ? (data) => options.onContainerPrepare!({ statuses: data.statuses ?? [] })
          : undefined,
        options.onError,
      ),
      remote_access: buildSSEListener<{ status: RemoteAccessStatus }>(
        'remote_access',
        options.onRemoteAccess
          ? (data) => options.onRemoteAccess!({ status: data.status })
          : undefined,
        options.onError,
      ),
      profile_sessions: buildSSEListener<{ profileSessions: ProfileSessions }>(
        'profile_sessions',
        options.onProfileSessions
          ? (data) => options.onProfileSessions!({ profileSessions: data.profileSessions })
          : undefined,
        options.onError,
      ),
      session_runtime: buildSSEListener<{
        sessionId: string;
        runtime: SessionRuntimeStatus;
      }>(
        'session_runtime',
        options.onSessionRuntime
          ? (data) =>
              options.onSessionRuntime!({
                sessionId: data.sessionId,
                runtime: data.runtime,
              })
          : undefined,
        options.onError,
      ),
      session_runtimes: buildSSEListener<{ sessions?: SessionRuntimeSnapshot[] }>(
        'session_runtimes',
        options.onSessionRuntimes
          ? (data) => options.onSessionRuntimes!({ sessions: data.sessions ?? [] })
          : undefined,
        options.onError,
      ),
      heartbeat: () => {
        options.onHeartbeat?.();
      },
    },
  });
}

// ── Browser Live View API ─────────────────────────────────

export interface BrowserStatus {
  running: boolean;
  cdpEndpoint: string | null;
  takeoverSessionId: string | null;
}

export async function fetchBrowserStatus(): Promise<BrowserStatus> {
  return request<BrowserStatus>('/api/browser/status');
}

export async function startBrowser(): Promise<BrowserStatus> {
  return request<BrowserStatus>('/api/browser/start', { method: 'POST' });
}

export async function stopBrowser(): Promise<BrowserStatus> {
  return request<BrowserStatus>('/api/browser/stop', { method: 'POST' });
}

/** Send an SDP offer and receive the SDP answer for WebRTC browser streaming. */
export async function browserOffer(sdp: string): Promise<string> {
  const res = await request<{ sdp: string }>('/api/browser/offer', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ sdp }),
  });
  return res.sdp;
}

export async function browserTakeoverStart(sessionId: string): Promise<void> {
  const value = sessionId.trim();
  if (!value) {
    throw new Error('browser takeover requires a non-empty session id');
  }

  await request<{ ok: boolean; sessionId: string }>('/api/browser/takeover/start', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ sessionId: value }),
  });
}

export async function browserTakeoverStop(sessionId: string): Promise<void> {
  const value = sessionId.trim();
  if (!value) {
    throw new Error('browser takeover requires a non-empty session id');
  }

  await request<{ ok: boolean; sessionId: string }>('/api/browser/takeover/stop', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ sessionId: value }),
  });
}

// ── Profile API ──────────────────────────────────────────

export async function fetchProfiles(): Promise<ProfileInfo[]> {
  return request<ProfileInfo[]>('/api/profiles');
}

export async function createProfile(input: {
  name: string;
  icon?: string;
  environment?: string;
  containerImage?: string;
  persona?: string;
  preferences?: Record<string, unknown>;
  locationAware?: boolean;
}): Promise<ProfileInfo> {
  return request<ProfileInfo>('/api/profiles', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function updateProfile(
  id: string,
  input: {
    name?: string;
    icon?: string;
    environment?: string;
    containerImage?: string;
    sortOrder?: number;
    persona?: string;
    preferences?: Record<string, unknown>;
    locationAware?: boolean;
  },
): Promise<ProfileInfo> {
  return request<ProfileInfo>(`/api/profiles/${encodeURIComponent(id)}`, {
    method: 'PUT',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function deleteProfile(id: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/profiles/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

export interface FetchProfileSessionsOptions {
  allowManagedPodmanDownload?: boolean;
}

export async function fetchProfileSessions(
  profileId: string,
  options?: FetchProfileSessionsOptions,
): Promise<ProfileSessions> {
  const params = new URLSearchParams();
  if (options?.allowManagedPodmanDownload) {
    params.set('allowManagedPodmanDownload', 'true');
  }
  const query = params.toString();
  const endpoint = query
    ? `/api/profiles/${encodeURIComponent(profileId)}/sessions?${query}`
    : `/api/profiles/${encodeURIComponent(profileId)}/sessions`;
  return request<ProfileSessions>(endpoint);
}

// ── Folder API ───────────────────────────────────────────

export async function createFolder(profileId: string, name: string): Promise<FolderInfo> {
  return request<FolderInfo>(
    `/api/profiles/${encodeURIComponent(profileId)}/folders`,
    {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ name }),
    },
  );
}

export async function updateFolder(
  id: string,
  input: { name?: string; sortOrder?: number },
): Promise<void> {
  await request<{ ok: boolean }>(`/api/folders/${encodeURIComponent(id)}`, {
    method: 'PUT',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function deleteFolder(id: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/folders/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

// ── Session pin/archive/move API ─────────────────────────

export async function pinSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/sessions/${encodeURIComponent(id)}/pin`,
    { method: 'POST' },
  );
}

export async function unpinSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/sessions/${encodeURIComponent(id)}/unpin`,
    { method: 'POST' },
  );
}

export async function archiveSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/sessions/${encodeURIComponent(id)}/archive`,
    { method: 'POST' },
  );
}

export async function unarchiveSession(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/sessions/${encodeURIComponent(id)}/unarchive`,
    { method: 'POST' },
  );
}

export async function moveSessionToFolder(
  sessionId: string,
  folderId: string | null,
): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/move-to-folder`,
    {
      method: 'POST',
      headers: JSON_HEADERS,
      body: JSON.stringify({ folderId }),
    },
  );
}

export async function fetchSessionTasks(sessionId: string): Promise<SessionTasksPayload> {
  return request<SessionTasksPayload>(
    `/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
  );
}

export async function saveSessionTasks(
  sessionId: string,
  payload: SessionTasksPayload,
): Promise<SessionTasksPayload> {
  return request<SessionTasksPayload>(
    `/api/sessions/${encodeURIComponent(sessionId)}/tasks`,
    {
      method: 'PUT',
      headers: JSON_HEADERS,
      body: JSON.stringify(payload),
    },
  );
}

// ── Scheduled Task API ──────────────────────────────────

export async function fetchTasks(): Promise<ScheduledTask[]> {
  return request<ScheduledTask[]>('/api/tasks');
}

export async function createTask(input: CreateTaskInput): Promise<ScheduledTask> {
  return request<ScheduledTask>('/api/tasks', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function fetchTask(id: string): Promise<ScheduledTask> {
  return request<ScheduledTask>(`/api/tasks/${encodeURIComponent(id)}`);
}

export async function updateTask(
  id: string,
  input: {
    name?: string;
    enabled?: boolean;
    cronExpression?: string;
    runAt?: string;
    scriptPath?: string;
    scriptInline?: string;
  },
): Promise<ScheduledTask> {
  return request<ScheduledTask>(`/api/tasks/${encodeURIComponent(id)}`, {
    method: 'PUT',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function deleteTask(id: string): Promise<void> {
  await request<{ ok: boolean }>(`/api/tasks/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

export async function approveTask(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/tasks/${encodeURIComponent(id)}/approve`,
    { method: 'POST' },
  );
}

export async function runTask(id: string): Promise<void> {
  await request<{ ok: boolean }>(
    `/api/tasks/${encodeURIComponent(id)}/run`,
    { method: 'POST' },
  );
}

// ── Web Push Notification API ───────────────────────────

export async function fetchVapidKey(): Promise<VapidKeyInfo> {
  return request<VapidKeyInfo>('/api/push/vapid-key');
}

export async function subscribePush(input: {
  endpoint: string;
  p256dh: string;
  auth: string;
}): Promise<void> {
  await request<{ ok: boolean }>('/api/push/subscribe', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(input),
  });
}

export async function unsubscribePush(endpoint: string): Promise<void> {
  await request<{ ok: boolean }>('/api/push/unsubscribe', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ endpoint }),
  });
}

export async function sendTestNotification(
  title?: string,
  body?: string,
): Promise<{ ok: boolean; sent: number }> {
  return request<{ ok: boolean; sent: number }>('/api/push/test', {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify({ title, body }),
  });
}
