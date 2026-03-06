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

export interface BashExecution {
  id: string;
  input: string;
  output?: string;
  status: 'running' | 'done' | 'failed';
  mode?: 'default' | 'unsafe' | 'ssh' | string;
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

export interface BashTimelineSegment {
  id: string;
  kind: 'bash';
  offset: number;
  commands: BashExecution[];
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
  | BashTimelineSegment
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
  bashExecutions?: BashExecution[];
  toolEvent?: ToolEvent;
  stats?: {
    providerId?: string;
    promptTokens?: number;
    completionTokens?: number;
    totalTokens?: number;
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
  projectId?: string;
  model: string;
  reasoningEffort?: string;
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
  type: 'local' | 'container';
  image?: string;
  containerId?: string;
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
}

export type PinnedItem =
  | { type: 'session' } & SessionSummary
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

// ── Web Push notification types ──────────────────────────

export interface VapidKeyInfo {
  publicKey: string;
}

export interface ScheduledTask {
  id: string;
  profileId: string;
  sessionId: string | null;
  name: string;
  triggerKind: 'time' | string;
  triggerType: 'cron' | 'once';
  cronExpression: string | null;
  runAt: string | null;
  scriptPath: string | null;
  scriptInline: string | null;
  needsNetwork: boolean;
  approved: boolean;
  enabled: boolean;
  lastRunAt: string | null;
  lastRunStatus: string | null;
  lastRunOutput: string | null;
  createdAt: string;
  updatedAt: string;
}
