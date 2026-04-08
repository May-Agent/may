// src/types/index.ts

export type Role = 'user' | 'assistant' | 'system';

export interface Attachment {
  id: string;
  name: string;
  type: 'image' | 'file' | 'document' | 'pdf';
  url: string;
  uploadDataUrl?: string;
  textContent?: string;
  textPreview?: string;
}

export interface TerminalExecution {
  id: string;
  description: string;
  input: string;
  output?: string;
  status: 'running' | 'done' | 'failed';
  mode?: 'default' | 'sandboxed' | 'unsafe' | 'ssh';
  startedAt?: number;
  endedAt?: number;
}

export interface ToolExecution {
  id: string;
  name: string;
  request?: string;
  response?: string;
  status: 'running' | 'done' | 'failed';
  startedAt?: number;
  endedAt?: number;
}

export interface ThinkingTimelineSegment {
  id: string;
  kind: 'thinking';
  offset: number;
  content: string;
  isRunning: boolean;
  startedAt?: number;
  endedAt?: number;
}

export interface TerminalTimelineSegment {
  id: string;
  kind: 'terminal';
  offset: number;
  commands: TerminalExecution[];
  startedAt?: number;
  endedAt?: number;
}

export interface ToolTimelineSegment {
  id: string;
  kind: 'tool';
  offset: number;
  tool: ToolExecution;
}

export type TimelineSegment =
  | ThinkingTimelineSegment
  | TerminalTimelineSegment
  | ToolTimelineSegment;

export interface ToolEvent {
  id: string;
  stage: 'start' | 'end';
  name?: string;
  arguments?: unknown;
  result?: unknown;
}

export interface Message {
  id: string;
  role: Role;
  content: string;
  timestamp: string;
  attachments?: Attachment[];
  isStreaming?: boolean;
  timelineSegments?: TimelineSegment[];
  thinking?: string;
  isThinking?: boolean;
  terminalExecutions?: TerminalExecution[];
  toolEvent?: ToolEvent;
  stats?: {
    providerId?: string;
    promptTokens?: number;
    completionTokens?: number;
    totalTokens?: number;
    cacheReadTokens?: number;
    cacheWriteTokens?: number;
    contextWindow?: number;
    elapsedMs?: number;
  };
  error?: string;
}

export interface Session {
  id: string;
  title: string;
  lastModified: string;
  preview: string;
  messages: Message[];
  profileId?: string | null;
  projectId?: string;
  model: string;
  reasoningEffort?: string;
  runtime: SessionRuntimeSummaryPayload;
}

export interface Artifact {
  path: string;
  name: string;
  size: number;
  isHtml: boolean;
  fingerprint?: string;
}

export type ArtifactViewerKind = 'default' | 'slidev';

export interface ArtifactPreviewTarget {
  path: string;
  viewer?: ArtifactViewerKind;
}

// ── Profile system types ─────────────────────────────────

export interface ProfileEnvironment {
  type: 'local' | 'container' | 'remote';
  image?: string;
  containerId?: string;
  sshServerId?: string;
}

export interface ProfileInfo {
  id: string;
  slug: string | null;
  name: string;
  icon: string | null;
  environment: ProfileEnvironment;
  persona: string | null;
  preferences: Record<string, unknown> | null;
  locationAware: boolean;
  pinnedLimit: number;
  sortOrder: number;
}

export interface SessionSummary {
  id: string;
  title: string | null;
  model: string | null;
  profileId: string | null;
  pinned: boolean;
  archived: boolean;
  folderId: string | null;
  pinSortOrder: number | null;
  messageCount: number;
  createdAt: string | null;
  updatedAt: string | null;
  contentPreview: string;
  runtime: SessionRuntimeSummaryPayload;
}

export type SessionPlanState = 'disabled' | 'draft' | 'approved';

export type SessionRuntimeState =
  | 'idle'
  | 'running'
  | 'waiting_permission'
  | 'waiting_workspace'
  | 'waiting_ask_user'
  | 'waiting_terminal_input';

export interface PendingUserMessage {
  content: string;
  attachments: Attachment[];
  hiddenContextXml?: string | null;
  createdAt: string;
  deliveryMode: PendingUserMessageMode;
}

export type PendingUserMessageMode = 'steer' | 'queue';

export interface RuntimePermissionRequest {
  id: string;
  mode: 'default' | 'unsafe' | 'ssh';
  script: string;
  sessionId?: string;
}

export interface RuntimeWorkspaceRequest {
  id: string;
  path: string;
  reason: string;
  sessionId?: string;
}

export interface RuntimeAskUserQuestion {
  section: string;
  question: string;
  option: string[];
  multiSelect: boolean;
}

export interface RuntimeAskUserRequest {
  id: string;
  questions: RuntimeAskUserQuestion[];
}

export interface SessionRuntimeSummaryPayload {
  state: SessionRuntimeState;
  planState: SessionPlanState;
  interactions?: {
    browser: {
      takeoverActive: boolean;
    };
    terminal?: {
      taskId?: string | null;
      outputFile?: string | null;
      outputPreview?: string | null;
      inputNotice?: string | null;
    };
  };
  queuedCount: number;
  pendingImmediate?: PendingUserMessage | null;
  queuedMessages: PendingUserMessage[];
  waitingPermission?: RuntimePermissionRequest | null;
  waitingWorkspace?: RuntimeWorkspaceRequest | null;
  waitingAskUser?: RuntimeAskUserRequest | null;
}

export type PinnedItem =
  | ({ type: 'session' } & SessionSummary)
  | {
      type: 'folder';
      id: string;
      name: string;
      sortOrder: number;
      sessions: SessionSummary[];
    };

export interface ProfileSessions {
  profile: ProfileInfo;
  pinned: PinnedItem[];
  regular: SessionSummary[];
  archived: SessionSummary[];
}

export interface FolderInfo {
  id: string;
  profileId: string;
  name: string;
  sortOrder: number;
}

// ── Scheduled task types ─────────────────────────────────

export type TaskTriggerKind = 'time' | 'event' | 'webhook';
export type TimeTriggerType = 'cron' | 'once';
export type TaskRunStatus = 'running' | 'success' | 'failed' | 'interrupted';

export type TaskTriggerInput =
  | {
      kind: 'time';
      triggerType: TimeTriggerType;
      cronUtc?: string;
      runAtUtc?: string;
    }
  | {
      kind: 'event';
      eventName: string;
    }
  | {
      kind: 'webhook';
    };

export type TaskTrigger =
  | {
      kind: 'time';
      triggerType: TimeTriggerType;
      cronUtc?: string | null;
      runAtUtc?: string | null;
    }
  | {
      kind: 'event';
      eventName: string;
    }
  | {
      kind: 'webhook';
      token: string;
      webhookPath: string;
    };

interface TaskCreateInputBase {
  profileId: string;
  sessionId?: string;
  name: string;
  scriptPath?: string;
  scriptInline?: string;
  needsNetwork?: boolean;
}

export type CreateTaskInput = TaskCreateInputBase & {
  trigger: TaskTriggerInput;
};

export interface ScheduledTask {
  id: string;
  profileId: string;
  sessionId: string | null;
  name: string;
  trigger: TaskTrigger;
  scriptPath: string | null;
  scriptInline: string | null;
  needsNetwork: boolean;
  approved: boolean;
  enabled: boolean;
  lastRunAt: string | null;
  lastRunStatus: TaskRunStatus | null;
  lastRunOutput: string | null;
  createdAt: string;
  updatedAt: string;
}

// ── Web Push notification types ──────────────────────────

export interface VapidKeyInfo {
  publicKey: string;
}
