/** Machine-readable error codes (snake_case) emitted by the Rust backend. */
export type ApiErrorCode =
  | 'bad_request'
  | 'internal_error'
  | 'not_found'
  | 'browser_takeover_active'
  | 'session_not_found'
  | 'model_not_configured'
  | 'provider_unavailable'
  | 'profile_configuration_invalid'
  | 'container_preparing'
  | 'container_build_failed'
  | 'container_start_failed'
  | 'container_runtime_unavailable'
  | 'managed_podman_approval_required'
  | 'location_unavailable';

/** Semantic variant of a tool call — server-assigned, never re-inferred client-side. */
export type ToolVariant =
  | 'terminal'
  | 'terminal_input'
  | 'terminal_kill'
  | 'terminal_read'
  | 'browser'
  | 'web_search'
  | 'web_fetch'
  | 'ask'
  | 'session'
  | 'notify'
  | 'task'
  | 'generic';

export type MessageRole = 'user' | 'assistant' | 'system';

export type AttachmentKind =
  | 'image'
  | 'document'
  | 'text'
  | 'file'
  | 'pdf'
  | 'audio'
  | 'video';

export interface ApiAttachment {
  id: string;
  name: string;
  type: AttachmentKind;
  url: string;
  uploadDataUrl?: string | null;
  textContent?: string | null;
}

export type TerminalCommandMode = 'default' | 'sandboxed' | 'unsafe' | 'ssh';
export type TerminalExecutionStatus = 'running' | 'done' | 'failed' | 'stopped';

export interface ApiTerminalExecution {
  id: string;
  description: string;
  input: string;
  output?: string | null;
  status: TerminalExecutionStatus;
  mode: TerminalCommandMode;
}

export interface RequestStats {
  providerId?: string | null;
  promptTokens?: number | null;
  completionTokens?: number | null;
  totalTokens?: number | null;
  cacheReadTokens?: number | null;
  cacheWriteTokens?: number | null;
  contextWindow?: number | null;
  elapsedMs?: number | null;
}

export interface ApiMessage {
  id: string;
  role: MessageRole;
  content: string;
  timestamp: string;
  attachments?: ApiAttachment[];
  thinking?: string | null;
  isStreaming?: boolean | null;
  isThinking?: boolean | null;
  toolEvent?: unknown;
  terminalExecutions?: ApiTerminalExecution[];
  stats?: RequestStats | null;
}

// ── Session runtime (mirrors src/session/runtime.rs) ────────

export type SessionRuntimeState =
  | 'idle'
  | 'running'
  | 'waiting_permission'
  | 'waiting_workspace'
  | 'waiting_ask_user'
  | 'waiting_terminal_input';

export type SessionPlanState = 'disabled' | 'draft' | 'approved';

export type PendingUserMessageMode = 'steer' | 'queue';

export interface BrowserInteractionState {
  takeoverActive?: boolean;
  takeoverAlertPending?: boolean;
}

export interface TerminalRuntimeState {
  taskId?: string | null;
  outputFile?: string | null;
  outputPreview?: string | null;
  inputNotice?: string | null;
  backgroundReason?: string | null;
}

export interface SessionInteractionState {
  browser?: BrowserInteractionState;
  terminal?: TerminalRuntimeState;
}

export interface PendingUserMessage {
  content: string;
  attachments?: ApiAttachment[];
  hiddenContextXml?: unknown;
  createdAt: string;
  deliveryMode: PendingUserMessageMode;
}

export interface PersistedPermissionRequestPayload {
  mode: string;
  script: string;
  sessionId?: string | null;
}

export interface PersistedPermissionRequest {
  id: string;
  request: PersistedPermissionRequestPayload;
}

export interface PersistedWorkspaceRequestPayload {
  path: string;
  reason: string;
  sessionId?: string | null;
}

export interface PersistedWorkspaceRequest {
  id: string;
  request: PersistedWorkspaceRequestPayload;
}

export interface PersistedAskUserQuestion {
  section: string;
  question: string;
  option: string[];
  multiSelect: boolean;
}

export interface PersistedAskUserRequest {
  id: string;
  questions: PersistedAskUserQuestion[];
}

export interface SessionRuntimeSummary {
  state?: SessionRuntimeState;
  planState?: SessionPlanState;
  interactions?: SessionInteractionState;
  queuedCount?: number;
  pendingImmediate?: PendingUserMessage | null;
  queuedMessages?: PendingUserMessage[];
  waitingPermission?: PersistedPermissionRequest | null;
  waitingWorkspace?: PersistedWorkspaceRequest | null;
  waitingAskUser?: PersistedAskUserRequest | null;
  updatedAt?: string | null;
}

export interface ApiSession {
  id: string;
  title: string;
  lastModified: string;
  preview: string;
  messages?: ApiMessage[];
  profileId: string | null;
  model: string;
  reasoningEffort?: string | null;
  runtime: SessionRuntimeSummary;
}

export interface ChatRequestPayload {
  sessionId: string;
  content: string;
  attachments?: ApiAttachment[];
  hiddenContextXml?: string;
  deliveryMode?: 'immediate' | 'queue';
}

// ── SSE events on /api/chat/stream ──────────────────────────

export interface SseStart {
  kind: 'start';
  seq: number;
  turnId: string;
  sessionId: string;
  assistantId: string;
}

export interface SseText {
  kind: 'text';
  seq: number;
  turnId: string;
  chunk: string;
}

export interface SseReasoning {
  kind: 'reasoning';
  seq: number;
  turnId: string;
  chunk: string;
}

export interface SseToolStart {
  kind: 'tool_start';
  seq: number;
  turnId: string;
  id: string;
  name: string;
  variant: ToolVariant;
  arguments: string;
}

export interface SseToolDelta {
  kind: 'tool_delta';
  seq: number;
  turnId: string;
  id: string;
  name: string;
  variant: ToolVariant;
  argumentsFragment: string;
}

export interface SseToolEnd {
  kind: 'tool_end';
  seq: number;
  turnId: string;
  id: string;
  name: string;
  variant: ToolVariant;
  arguments?: string;
  result: unknown;
}

export interface SseTodoItem {
  text: string;
  checked: boolean;
}

export interface SseTodoUpdate {
  kind: 'todo_update';
  seq: number;
  turnId: string;
  items: SseTodoItem[];
  total: number;
  completed: number;
}

export interface SseError {
  kind: 'error';
  seq: number;
  turnId: string;
  code: ApiErrorCode;
  message: string;
  retryable: boolean;
}

export interface SseDone {
  kind: 'done';
  seq: number;
  turnId: string;
  sessionId: string;
  assistant: ApiMessage;
}

export interface SsePing {
  kind: 'ping';
  seq: number;
  turnId: string;
}

export type ChatStreamEvent =
  | SseStart
  | SseText
  | SseReasoning
  | SseToolStart
  | SseToolDelta
  | SseToolEnd
  | SseTodoUpdate
  | SseError
  | SseDone
  | SsePing;

// ── UI event stream /api/events ─────────────────────────────

export interface BrowserStatusPayload {
  enabled: boolean;
  running: boolean;
  cdpEndpoint?: string | null;
  ownerSessionId?: string | null;
  takeoverSessionId?: string | null;
}

export interface ProfileSessions {
  profileId: string;
  pinned: Array<
    | { type: 'session'; id: string; title: string; runtime: SessionRuntimeSummary }
    | {
        type: 'folder';
        id: string;
        name: string;
        sessions: Array<{
          id: string;
          title: string;
          runtime: SessionRuntimeSummary;
        }>;
      }
  >;
  regular: Array<{ id: string; title: string; runtime: SessionRuntimeSummary; profileId?: string | null }>;
  archived: Array<{ id: string; title: string; runtime: SessionRuntimeSummary; profileId?: string | null }>;
}

export type ProfileSessionsPayload = {
  profileSessions: ProfileSessions;
};

export interface SessionRuntimePayload {
  sessionId: string;
  runtime: SessionRuntimeSummary;
}

export interface SessionRuntimesPayload {
  sessions: SessionRuntimePayload[];
}

export type UiEvent =
  | { kind: 'heartbeat'; ok: boolean }
  | { kind: 'config'; payload: unknown }
  | { kind: 'browser_status'; payload: BrowserStatusPayload }
  | { kind: 'profile_sessions'; payload: ProfileSessionsPayload }
  | { kind: 'session_runtime'; payload: SessionRuntimePayload }
  | { kind: 'session_runtimes'; payload: SessionRuntimesPayload }
  | { kind: 'container_prepare'; payload: unknown }
  | { kind: 'remote_access'; payload: unknown }
  | { kind: 'request_logs'; payload: unknown }
  | { kind: 'notification_mailbox'; payload: unknown }
  | { kind: 'network_access_logs'; payload: unknown };

// ── Profiles ────────────────────────────────────────────────

export interface ApiProfile {
  id: string;
  name: string;
  createdAt: string;
  updatedAt: string;
  defaultModel?: string | null;
  description?: string | null;
}

export interface ProfilesResponse {
  items: ApiProfile[];
  currentProfileId: string | null;
}
