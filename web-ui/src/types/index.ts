// src/types/index.ts

export type Role = 'user' | 'assistant' | 'system';

export interface Attachment {
  id: string;
  name: string;
  type: 'image' | 'file';
  url: string;
}

export interface Message {
  id: string;
  role: Role;
  content: string;
  timestamp: string;
  attachments?: Attachment[];
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

export interface User {
  id: string;
  name: string;
  avatar: string;
}
