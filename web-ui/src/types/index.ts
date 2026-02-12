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
  startedAt?: number;
  endedAt?: number;
}

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
  reasoningEffort?: 'low' | 'medium' | 'high';
}

export interface Project {
  id: string;
  name: string;
  description?: string;
  createdAt: string;
}

export interface Artifact {
  path: string;
  name: string;
  size: number;
  isHtml: boolean;
  fingerprint?: string;
}


