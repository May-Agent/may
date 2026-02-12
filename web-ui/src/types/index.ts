// src/types/index.ts

export type ModelKind = 'llm' | 'image' | 'embedding' | 'reranker';


export type Role = 'user' | 'assistant' | 'system';

export interface Attachment {
  id: string;
  name: string;
  type: 'image' | 'file' | 'document' | 'pdf';
  url: string;
  textContent?: string;
  textPreview?: string;
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

export interface Project {
  id: string;
  name: string;
  description?: string;
  createdAt: string;
}


