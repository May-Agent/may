// Tool request/response parsing — extracted from MessageItem.tsx and ChatInterface.tsx
// Pure functions, zero React dependencies.

import type { TerminalExecution } from '@/types';

// ── Tool name helpers ──────────────────────────────────────

export function normalizeToolName(name: string): string {
  return name.trim().toLowerCase();
}

export function isWebSearchToolName(name: string): boolean {
  return (
    name === 'websearch' ||
    name.endsWith('.websearch') ||
    name.endsWith('/websearch')
  );
}

export function isWebFetchToolName(name: string): boolean {
  return (
    name === 'webfetch' ||
    name.endsWith('.webfetch') ||
    name.endsWith('/webfetch')
  );
}

export function isBrowserToolName(name: string): boolean {
  return (
    name === 'browser' ||
    name.endsWith('.browser') ||
    name.endsWith('/browser')
  );
}

export function isTerminalExecutionMode(value: unknown): value is TerminalExecution['mode'] {
  return (
    value === 'default' ||
    value === 'sandboxed' ||
    value === 'unsafe' ||
    value === 'ssh'
  );
}

// ── Terminal tool start args ───────────────────────────────

export interface TerminalToolStartArgs {
  description: string;
  script: string;
  mode?: TerminalExecution['mode'];
}

export function parseTerminalToolStartArgs(raw: unknown): TerminalToolStartArgs {
  const parsed =
    typeof raw === 'string'
      ? JSON.parse(raw)
      : raw;
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('terminal tool payload must be an object');
  }
  const record = parsed as Record<string, unknown>;
  const description =
    typeof record.description === 'string' ? record.description.trim() : '';
  if (!description) {
    throw new Error('terminal tool payload is missing a non-empty description');
  }
  const script =
    typeof record.script === 'string'
      ? record.script
      : typeof record.command === 'string'
        ? record.command
        : '';
  if (!script) {
    throw new Error('terminal tool payload is missing script');
  }
  const mode = isTerminalExecutionMode(record.mode) ? record.mode : undefined;
  return { description, script, mode };
}

// ── JSON parsing helper ────────────────────────────────────

function parseJsonLike(value: string): unknown {
  const trimmed = value.trim();
  if (!trimmed) {
    return value;
  }
  const looksLikeJson =
    (trimmed.startsWith('{') && trimmed.endsWith('}')) ||
    (trimmed.startsWith('[') && trimmed.endsWith(']'));
  if (!looksLikeJson) {
    return value;
  }
  try {
    return JSON.parse(trimmed);
  } catch {
    return value;
  }
}

function parseJsonRecord(raw: string | undefined): Record<string, unknown> | null {
  if (!raw || raw.trim().length === 0) {
    return null;
  }
  const parsed = parseJsonLike(raw);
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    return null;
  }
  return parsed as Record<string, unknown>;
}

// ── Web search ─────────────────────────────────────────────

export interface WebSearchResultEntry {
  title: string;
  url: string;
  snippet: string;
  host: string;
}

export function parseWebSearchQuery(rawRequest: string | undefined): string {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return '';
  }
  const query = parsed.query;
  return typeof query === 'string' ? query.trim() : '';
}

function extractHost(urlValue: string): string {
  try {
    return new URL(urlValue).hostname;
  } catch {
    return urlValue;
  }
}

export function parseWebSearchResults(rawResponse: string | undefined): WebSearchResultEntry[] {
  if (!rawResponse || rawResponse.trim().length === 0) {
    return [];
  }
  const parsed = parseJsonLike(rawResponse);
  const candidates = Array.isArray(parsed)
    ? parsed
    : parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? Array.isArray((parsed as Record<string, unknown>).results)
        ? ((parsed as Record<string, unknown>).results as unknown[])
        : []
      : [];

  return candidates.flatMap((entry) => {
    if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
      return [];
    }
    const record = entry as Record<string, unknown>;
    const title = typeof record.title === 'string' ? record.title.trim() : '';
    const url = typeof record.url === 'string' ? record.url.trim() : '';
    const snippet = typeof record.snippet === 'string' ? record.snippet.trim() : '';
    if (!title || !url) {
      return [];
    }
    return [{
      title,
      url,
      snippet,
      host: extractHost(url),
    }];
  });
}

// ── Web fetch ──────────────────────────────────────────────

export function parseWebFetchUrl(rawRequest: string | undefined): string {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return '';
  }
  const urlCandidate = parsed.url ?? parsed.uri ?? parsed.link;
  return typeof urlCandidate === 'string' ? urlCandidate.trim() : '';
}

// ── Browser ────────────────────────────────────────────────

export type BrowserAction = 'status' | 'start' | 'stop' | 'cdp-endpoint' | 'read' | 'unknown';

export interface BrowserPageDisplay {
  url: string;
  title: string;
  mainHeading?: string;
  textExcerpt: string;
  headings: string[];
  interactiveElements: Array<{
    role: string;
    tagName: string;
    text?: string;
    accessibleName?: string;
    href?: string;
    inputType?: string;
    disabled: boolean;
  }>;
  accessibility: {
    totalNodes: number;
    interestingNodes: number;
    interactiveNodes: number;
    headingNodes: number;
    landmarkNodes: number;
  };
  screenshot?: {
    apiPath: string;
    artifactPath: string;
    mimeType: string;
    width: number;
    height: number;
  };
}

export interface BrowserToolDisplayResult {
  running: boolean;
  cdpEndpoint?: string;
  page?: BrowserPageDisplay;
}

export function parseBrowserAction(rawRequest: string | undefined): BrowserAction {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return 'unknown';
  }
  const command = parsed.command;
  if (typeof command !== 'string') {
    return 'unknown';
  }
  const normalized = command.trim().toLowerCase();
  if (
    normalized === 'status' ||
    normalized === 'start' ||
    normalized === 'stop' ||
    normalized === 'cdp-endpoint' ||
    normalized === 'read'
  ) {
    return normalized;
  }
  return 'unknown';
}

export function parseBrowserUrl(rawRequest: string | undefined): string {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return '';
  }
  const url = parsed.url;
  return typeof url === 'string' ? url.trim() : '';
}

export function parseBrowserResult(
  rawResponse: string | undefined,
): BrowserToolDisplayResult | undefined {
  const parsed = parseJsonRecord(rawResponse);
  if (!parsed) {
    return undefined;
  }
  const running = parsed.running;
  if (typeof running !== 'boolean') {
    return undefined;
  }
  const cdpEndpoint =
    typeof parsed.cdpEndpoint === 'string' && parsed.cdpEndpoint.trim().length > 0
      ? parsed.cdpEndpoint.trim()
      : undefined;
  const pageNode =
    parsed.page && typeof parsed.page === 'object' && !Array.isArray(parsed.page)
      ? (parsed.page as Record<string, unknown>)
      : undefined;
  const screenshotNode =
    pageNode?.screenshot && typeof pageNode.screenshot === 'object' && !Array.isArray(pageNode.screenshot)
      ? (pageNode.screenshot as Record<string, unknown>)
      : undefined;
  const accessibilityNode =
    pageNode?.accessibility && typeof pageNode.accessibility === 'object' && !Array.isArray(pageNode.accessibility)
      ? (pageNode.accessibility as Record<string, unknown>)
      : undefined;
  const interactiveElements = Array.isArray(pageNode?.interactiveElements)
    ? pageNode?.interactiveElements.flatMap((entry) => {
        if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
          return [];
        }
        const node = entry as Record<string, unknown>;
        const role = typeof node.role === 'string' ? node.role.trim() : '';
        const tagName = typeof node.tagName === 'string' ? node.tagName.trim() : '';
        if (!role || !tagName) {
          return [];
        }
        return [{
          role,
          tagName,
          text: typeof node.text === 'string' ? node.text.trim() || undefined : undefined,
          accessibleName:
            typeof node.accessibleName === 'string'
              ? node.accessibleName.trim() || undefined
              : undefined,
          href: typeof node.href === 'string' ? node.href.trim() || undefined : undefined,
          inputType:
            typeof node.inputType === 'string'
              ? node.inputType.trim() || undefined
              : undefined,
          disabled: node.disabled === true,
        }];
      })
    : [];

  const page =
    pageNode &&
    typeof pageNode.url === 'string' &&
    typeof pageNode.title === 'string' &&
    typeof pageNode.textExcerpt === 'string'
      ? {
          url: pageNode.url.trim(),
          title: pageNode.title.trim(),
          mainHeading:
            typeof pageNode.mainHeading === 'string'
              ? pageNode.mainHeading.trim() || undefined
              : undefined,
          textExcerpt: pageNode.textExcerpt.trim(),
          headings: Array.isArray(pageNode.headings)
            ? pageNode.headings.flatMap((heading) =>
                typeof heading === 'string' && heading.trim().length > 0
                  ? [heading.trim()]
                  : [],
              )
            : [],
          interactiveElements,
          accessibility: {
            totalNodes:
              typeof accessibilityNode?.totalNodes === 'number'
                ? accessibilityNode.totalNodes
                : 0,
            interestingNodes:
              typeof accessibilityNode?.interestingNodes === 'number'
                ? accessibilityNode.interestingNodes
                : 0,
            interactiveNodes:
              typeof accessibilityNode?.interactiveNodes === 'number'
                ? accessibilityNode.interactiveNodes
                : 0,
            headingNodes:
              typeof accessibilityNode?.headingNodes === 'number'
                ? accessibilityNode.headingNodes
                : 0,
            landmarkNodes:
              typeof accessibilityNode?.landmarkNodes === 'number'
                ? accessibilityNode.landmarkNodes
                : 0,
          },
          screenshot:
            screenshotNode &&
            typeof screenshotNode.apiPath === 'string' &&
            typeof screenshotNode.artifactPath === 'string' &&
            typeof screenshotNode.mimeType === 'string' &&
            typeof screenshotNode.width === 'number' &&
            typeof screenshotNode.height === 'number'
              ? {
                  apiPath: screenshotNode.apiPath.trim(),
                  artifactPath: screenshotNode.artifactPath.trim(),
                  mimeType: screenshotNode.mimeType.trim(),
                  width: screenshotNode.width,
                  height: screenshotNode.height,
                }
              : undefined,
        }
      : undefined;

  return {
    running,
    cdpEndpoint,
    page,
  };
}

// ── Summary builders ───────────────────────────────────────

export function toSingleLineSummary(text: string, limit = 92): string {
  const normalized = text.replace(/\s+/g, ' ').trim();
  if (!normalized) {
    return '';
  }
  if (normalized.length <= limit) {
    return normalized;
  }
  return `${normalized.slice(0, Math.max(0, limit - 1)).trimEnd()}…`;
}

export function buildBrowserSummary(
  action: BrowserAction,
  url: string,
  result: BrowserToolDisplayResult | undefined,
): string {
  if (action === 'read') {
    if (result?.page?.title) {
      return result.page.title;
    }
    if (url) {
      return toSingleLineSummary(url, 96) || 'Browser read';
    }
    return 'Browser read';
  }
  if (action === 'start') {
    return 'Opened browser';
  }
  if (action === 'stop') {
    return 'Closed browser';
  }
  if (action === 'status') {
    return result?.running ? 'Browser running' : 'Browser stopped';
  }
  if (action === 'cdp-endpoint') {
    return 'Browser CDP endpoint';
  }
  return 'Browser';
}

export function buildBrowserRunningLabel(
  action: BrowserAction,
  url: string,
): string {
  if (action === 'read') {
    return url ? `Reading ${url}` : 'Reading page in browser';
  }
  if (action === 'start') {
    return 'Opening browser';
  }
  if (action === 'stop') {
    return 'Closing browser';
  }
  if (action === 'status') {
    return 'Checking browser status';
  }
  if (action === 'cdp-endpoint') {
    return 'Reading browser CDP endpoint';
  }
  return 'Using browser';
}
