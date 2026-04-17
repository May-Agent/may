// Timeline display item types and construction — extracted from MessageItem.tsx
// Pure functions, zero React dependencies.

import type { Message, TerminalExecution } from '@/types';
import {
  type ParsedCommandOutput,
  parseCommandOutput,
  detectCodeLanguage,
  formatCodeText,
} from './parsers/toolOutput';
import {
  type WebSearchResultEntry,
  type BrowserAction,
  type BrowserToolDisplayResult,
  normalizeToolName,
  isWebSearchToolName,
  isWebFetchToolName,
  isBrowserToolName,
  parseWebSearchQuery,
  parseWebSearchResults,
  parseWebFetchUrl,
  parseBrowserAction,
  parseBrowserUrl,
  parseBrowserResult,
  buildBrowserSummary,
  buildBrowserRunningLabel,
  toSingleLineSummary,
} from './parsers/toolRequest';

// ── Timeline display item types ────────────────────────────

export interface ThinkingTimelineDisplayItem {
  id: string;
  type: 'thinking';
  summary: string;
  detail: string;
  isRunning: boolean;
  isFailed: boolean;
  startedAt?: number;
  endedAt?: number;
}

export interface CommandTimelineDisplayItem {
  id: string;
  type: 'command';
  summary: string;
  description: string;
  commandInput: string;
  parsedOutput: ParsedCommandOutput;
  outputText: string;
  isRunning: boolean;
  isFailed: boolean;
  status: TerminalExecution['status'];
  taskId?: string;
  errorMessage?: string;
  startedAt?: number;
  endedAt?: number;
}

export interface WebSearchTimelineDisplayItem {
  id: string;
  type: 'websearch';
  summary: string;
  query: string;
  requestRaw?: string;
  responseRaw?: string;
  results: WebSearchResultEntry[];
  isRunning: boolean;
  isFailed: boolean;
  status: 'running' | 'done' | 'failed' | 'stopped';
  startedAt?: number;
  endedAt?: number;
}

export interface WebFetchTimelineDisplayItem {
  id: string;
  type: 'webfetch';
  summary: string;
  url: string;
  requestRaw?: string;
  responseRaw?: string;
  isRunning: boolean;
  isFailed: boolean;
  status: 'running' | 'done' | 'failed' | 'stopped';
  startedAt?: number;
  endedAt?: number;
}

export interface BrowserTimelineDisplayItem {
  id: string;
  type: 'browser';
  summary: string;
  runningLabel: string;
  action: BrowserAction;
  url?: string;
  requestRaw?: string;
  responseRaw?: string;
  result?: BrowserToolDisplayResult;
  isRunning: boolean;
  isFailed: boolean;
  status: 'running' | 'done' | 'failed' | 'stopped';
  startedAt?: number;
  endedAt?: number;
}

export type TimelineDisplayItem =
  | ThinkingTimelineDisplayItem
  | CommandTimelineDisplayItem
  | WebSearchTimelineDisplayItem
  | WebFetchTimelineDisplayItem
  | BrowserTimelineDisplayItem;

// ── Timeline label helpers ─────────────────────────────────

function truncate(value: string, max: number): string {
  const trimmed = value.trim();
  if (trimmed.length <= max) return trimmed;
  return `${trimmed.slice(0, Math.max(0, max - 1))}…`;
}

function extractHostname(raw: string | undefined): string {
  if (!raw) return '';
  const trimmed = raw.trim();
  if (trimmed.length === 0) return '';
  try {
    const url = new URL(trimmed);
    return url.hostname || trimmed;
  } catch {
    return trimmed;
  }
}

export function buildRunningTimelineLabel(item: TimelineDisplayItem): string {
  if (item.type === 'thinking') {
    return 'Thinking';
  }
  if (item.type === 'command') {
    const description = item.description.trim();
    return description.length > 0
      ? description
      : item.commandInput.trim().length > 0
        ? `$ ${truncate(item.commandInput, 60)}`
        : 'Running command';
  }
  if (item.type === 'websearch') {
    const query = item.query.trim();
    return query.length > 0 ? `Searching "${truncate(query, 40)}"` : 'Searching the web';
  }
  if (item.type === 'webfetch') {
    const host = extractHostname(item.url);
    return host ? `Fetching ${host}` : 'Fetching page';
  }
  if (item.type === 'browser') {
    return item.runningLabel;
  }
  return 'Fetching page';
}

export function buildCollapsedTimelineLabel(item: TimelineDisplayItem): string {
  if (item.type === 'thinking') {
    const detail = truncate(item.detail, 60);
    return detail.length > 0 ? `Thinking · ${detail}` : 'Thinking';
  }
  if (item.type === 'command') {
    const description = item.description.trim();
    if (description.length > 0) return description;
    const script = item.commandInput.trim();
    return script.length > 0 ? `$ ${truncate(script, 60)}` : 'Command';
  }
  if (item.type === 'websearch') {
    if (item.status === 'failed') {
      const query = item.query.trim();
      return query.length > 0 ? `Search failed: "${truncate(query, 36)}"` : 'Search failed';
    }
    const count = item.results.length;
    if (count > 0) {
      return `Searched the web · ${count} result${count === 1 ? '' : 's'}`;
    }
    const query = item.query.trim();
    return query.length > 0 ? `Searched "${truncate(query, 40)}"` : 'Web search';
  }
  if (item.type === 'webfetch') {
    const host = extractHostname(item.url);
    if (item.status === 'failed') {
      return host ? `Fetch failed: ${host}` : 'Fetch failed';
    }
    return host ? `Fetched ${host}` : 'Web fetch';
  }
  if (item.type === 'browser') {
    if (item.status === 'failed') {
      return `${item.summary} failed`;
    }
    return item.summary;
  }
  return 'Web fetch';
}

export function timelineItemStatusLabel(item: TimelineDisplayItem): string | null {
  if (item.type === 'thinking' || item.isRunning) {
    return null;
  }
  if (
    item.type === 'command' &&
    item.status === 'failed' &&
    typeof item.parsedOutput.exitCode === 'number'
  ) {
    return `exit ${item.parsedOutput.exitCode}`;
  }
  if (item.status === 'failed') {
    return 'failed';
  }
  if (item.status === 'stopped') {
    return 'stopped';
  }
  return null;
}

export function timelineItemStatusClass(item: TimelineDisplayItem): string {
  if (item.type === 'thinking') {
    return 'text-muted-foreground';
  }
  if (item.isRunning) {
    return 'timeline-running-label';
  }
  if (item.status === 'failed') {
    return 'text-destructive';
  }
  if (item.status === 'stopped') {
    return 'text-amber-600 dark:text-amber-300';
  }
  return 'text-foreground';
}

// ── Duration formatting ────────────────────────────────────

export function formatDurationMs(durationMs: number): string {
  const safeDuration = Math.max(0, Math.round(durationMs));
  const totalSeconds = Math.max(1, Math.floor(safeDuration / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}m ${seconds.toString().padStart(2, '0')}s`;
}

// ── Build timeline items from a message ────────────────────

function buildCommandItem(
  command: TerminalExecution,
  fallbackStartedAt: number | undefined,
  segmentStartedAt?: number,
  segmentEndedAt?: number,
): CommandTimelineDisplayItem {
  const parsedOutput = parseCommandOutput(command.output);
  const outputLang = detectCodeLanguage(parsedOutput.text || '');
  return {
    id: command.id,
    type: 'command',
    summary: toSingleLineSummary(command.input, 96) || '(empty command)',
    description: command.description,
    commandInput: command.input,
    parsedOutput,
    outputText: parsedOutput.text
      ? formatCodeText(parsedOutput.text, outputLang)
      : '',
    isRunning: command.status === 'running',
    isFailed: command.status === 'failed',
    status: command.status,
    taskId: parsedOutput.taskId,
    errorMessage: parsedOutput.errorMessage,
    startedAt: command.startedAt ?? segmentStartedAt ?? fallbackStartedAt,
    endedAt: command.endedAt ?? segmentEndedAt,
  };
}

export function buildTimelineDisplayItems(message: Message): TimelineDisplayItem[] {
  const timestampMs = Date.parse(message.timestamp);
  const fallbackStartedAt = Number.isFinite(timestampMs) ? timestampMs : undefined;
  const segments = message.timelineSegments ?? [];
  const items: TimelineDisplayItem[] = [];

  if (segments.length === 0) {
    if (typeof message.thinking === 'string' && message.thinking.trim().length > 0) {
      items.push({
        id: `${message.id}-thinking-fallback`,
        type: 'thinking',
        summary: toSingleLineSummary(message.thinking, 90) || 'Thinking',
        detail: message.thinking,
        isRunning: !!message.isThinking,
        isFailed: false,
        startedAt: fallbackStartedAt,
        endedAt: message.isThinking ? undefined : fallbackStartedAt,
      });
    }
    for (const command of message.terminalExecutions ?? []) {
      items.push(buildCommandItem(command, fallbackStartedAt));
    }
    return items;
  }

  for (const segment of segments) {
    if (segment.kind === 'thinking') {
      items.push({
        id: segment.id,
        type: 'thinking',
        summary: toSingleLineSummary(segment.content, 90) || 'Thinking',
        detail: segment.content,
        isRunning: segment.isRunning,
        isFailed: false,
        startedAt: segment.startedAt,
        endedAt: segment.endedAt,
      });
      continue;
    }
    if (segment.kind === 'terminal') {
      for (const command of segment.commands) {
        items.push(buildCommandItem(command, fallbackStartedAt, segment.startedAt, segment.endedAt));
      }
      continue;
    }
    const toolName = normalizeToolName(segment.tool.name);
    if (isWebSearchToolName(toolName)) {
      const query = parseWebSearchQuery(segment.tool.request);
      const results = parseWebSearchResults(segment.tool.response);
      const summary = query
        ? toSingleLineSummary(query, 96)
        : 'Web search';
      items.push({
        id: segment.id,
        type: 'websearch',
        summary: summary || 'Web search',
        query,
        requestRaw: segment.tool.request,
        responseRaw: segment.tool.response,
        results,
        isRunning: segment.tool.status === 'running',
        isFailed: segment.tool.status === 'failed',
        status: segment.tool.status,
        startedAt: segment.tool.startedAt,
        endedAt: segment.tool.endedAt,
      });
    } else if (isWebFetchToolName(toolName)) {
      const url = parseWebFetchUrl(segment.tool.request);
      const summary = url
        ? toSingleLineSummary(url, 96)
        : 'Web fetch';
      items.push({
        id: segment.id,
        type: 'webfetch',
        summary: summary || 'Web fetch',
        url,
        requestRaw: segment.tool.request,
        responseRaw: segment.tool.response,
        isRunning: segment.tool.status === 'running',
        isFailed: segment.tool.status === 'failed',
        status: segment.tool.status,
        startedAt: segment.tool.startedAt,
        endedAt: segment.tool.endedAt,
      });
    } else if (isBrowserToolName(toolName)) {
      const action = parseBrowserAction(segment.tool.request);
      const url = parseBrowserUrl(segment.tool.request);
      const result = parseBrowserResult(segment.tool.response);
      items.push({
        id: segment.id,
        type: 'browser',
        summary: buildBrowserSummary(action, url, result),
        runningLabel: buildBrowserRunningLabel(action, url),
        action,
        url,
        requestRaw: segment.tool.request,
        responseRaw: segment.tool.response,
        result,
        isRunning: segment.tool.status === 'running',
        isFailed: segment.tool.status === 'failed',
        status: segment.tool.status,
        startedAt: segment.tool.startedAt,
        endedAt: segment.tool.endedAt,
      });
    }
  }
  return items;
}
