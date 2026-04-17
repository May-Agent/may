import type {
  ArtifactPreviewTarget,
  ArtifactViewerKind,
  TerminalExecution,
  Message,
  TimelineSegment,
} from "@/types";
import type { SessionContextDebugSnapshot } from "@/lib/api";

// ── Types ────────────────────────────────────────────────

export interface QueuedMessage {
  content: string;
  attachments: import("@/types").Attachment[];
}

export interface PreviewPaneState {
  kind: 'iframe' | 'image' | 'audio' | 'markdown' | 'text' | 'external' | 'slidev';
  url?: string;
  markdown?: string;
  text?: string;
}

export interface ParsedToolResult {
  output?: string;
  status: "done" | "failed" | "stopped";
  /** Backend terminal task_id, only present for backgrounded terminal tools. */
  taskId?: string;
}

export interface ContextRoleTokenBucket {
  role: string;
  label: string;
  messages: number;
  tokens: number;
}

export interface ContextRoleTokenBreakdown {
  buckets: ContextRoleTokenBucket[];
  total: number;
}

export type ContextDebugSnapshotLoadResult =
  | { ok: true; payload: import("@/lib/api").SessionContextDebugPayload }
  | { ok: false; error: string };

// ── Constants ────────────────────────────────────────────

export const ARTIFACT_PANEL_MIN_WIDTH = 420;
export const ARTIFACT_PANEL_DEFAULT_WIDTH = 680;
export const ARTIFACT_PANEL_MIN_CHAT_WIDTH = 560;
export const ARTIFACT_PANEL_MAX_VIEWPORT_RATIO = 0.72;
export const LONG_TEXT_THRESHOLD = 500;

const CONTEXT_ROLE_ORDER = ["system", "user", "assistant", "tool"] as const;
const ARTIFACT_TAG_PATTERN = /<artifact\b([^>]*)\/?>/gi;
const ARTIFACT_ATTRIBUTE_PATTERN =
  /([A-Za-z_][\w:-]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;

type ArtifactAttributeValue = string | true;
type ArtifactAttributes = Record<string, ArtifactAttributeValue>;

function parseArtifactAttributes(raw: string): ArtifactAttributes {
  const attributes: ArtifactAttributes = {};
  for (const match of raw.matchAll(ARTIFACT_ATTRIBUTE_PATTERN)) {
    const key = match[1]?.trim().toLowerCase();
    if (!key) continue;
    const value = match[2] ?? match[3] ?? match[4];
    attributes[key] = value === undefined ? true : value;
  }
  return attributes;
}

function isTruthyAttribute(value: ArtifactAttributeValue | undefined): boolean {
  if (value === true) {
    return true;
  }
  if (typeof value !== "string") {
    return false;
  }
  const normalized = value.trim().toLowerCase();
  return (
    normalized.length > 0 &&
    normalized !== "0" &&
    normalized !== "false" &&
    normalized !== "no" &&
    normalized !== "off"
  );
}

function parseArtifactViewer(attributes: ArtifactAttributes): ArtifactViewerKind | undefined {
  if (isTruthyAttribute(attributes.slidev)) {
    return "slidev";
  }
  const viewer = attributes.viewer;
  if (typeof viewer === "string" && viewer.trim().toLowerCase() === "slidev") {
    return "slidev";
  }
  const kind = attributes.type;
  if (typeof kind === "string" && kind.trim().toLowerCase() === "slidev") {
    return "slidev";
  }
  return undefined;
}

export function extractArtifactPreviewTargets(content: string): ArtifactPreviewTarget[] {
  const targets: ArtifactPreviewTarget[] = [];
  for (const match of content.matchAll(ARTIFACT_TAG_PATTERN)) {
    const attributes = parseArtifactAttributes(match[1] ?? "");
    const rawPath = attributes.url;
    if (typeof rawPath !== "string") {
      continue;
    }
    const path = rawPath.trim();
    if (!path) {
      continue;
    }
    targets.push({
      path,
      viewer: parseArtifactViewer(attributes),
    });
  }
  return targets;
}

export function stripArtifactPreviewTags(content: string): string {
  return content.replace(/\s*<artifact\b[^>]*\/?>\s*/gi, "\n").trim();
}

export function buildArtifactViewerHints(
  messages: Message[],
  normalizePath: (path: string) => string,
): Map<string, ArtifactPreviewTarget["viewer"]> {
  const hints = new Map<string, ArtifactPreviewTarget["viewer"]>();
  for (const message of messages) {
    if (message.role !== "assistant") {
      continue;
    }
    const targets = extractArtifactPreviewTargets(message.content);
    for (const target of targets) {
      const normalizedPath = normalizePath(target.path);
      if (!normalizedPath) {
        continue;
      }
      if (hints.has(normalizedPath)) {
        hints.delete(normalizedPath);
      }
      hints.set(normalizedPath, target.viewer);
    }
  }
  return hints;
}

// ── Disconnect banner helper ─────────────────────────────

export function createDisconnectBanner(
  setError: React.Dispatch<React.SetStateAction<string | null>>,
  message: string,
) {
  let timer: number | null = null;
  return {
    clear() {
      if (timer != null) {
        window.clearTimeout(timer);
        timer = null;
      }
      setError((prev) => (prev === message ? null : prev));
    },
    schedule() {
      if (timer != null) return;
      timer = window.setTimeout(() => {
        timer = null;
        setError(message);
      }, 1200);
    },
    cleanup() {
      if (timer != null) {
        window.clearTimeout(timer);
      }
    },
  };
}

// ── Retry helpers ────────────────────────────────────────

export function collectRetryBatch(
  messages: import("@/types").Message[],
  assistantId: string,
): QueuedMessage[] {
  const index = messages.findIndex((m) => m.id === assistantId);
  if (index <= 0) return [];

  const batch: QueuedMessage[] = [];
  for (let i = index - 1; i >= 0; i--) {
    const msg = messages[i];
    if (msg.role !== "user") break;
    batch.unshift({
      content: msg.content,
      attachments: msg.attachments ?? [],
    });
  }
  return batch;
}

// ── Context debug utilities ──────────────────────────────

export function estimateTextTokens(value: string): number {
  const text = value.trim();
  if (!text) {
    return 0;
  }
  return Math.max(1, Math.ceil(text.length / 4));
}

export function estimateMessageTokens(
  message: SessionContextDebugSnapshot["assembledMessages"][number],
): number {
  let total = estimateTextTokens(message.content) + 4;
  for (const attachment of message.attachments) {
    total += estimateTextTokens(attachment) + 2;
  }
  if (message.toolCallId) {
    total += estimateTextTokens(message.toolCallId) + 2;
  }
  for (const toolCallName of message.toolCallNames) {
    total += estimateTextTokens(toolCallName) + 2;
  }
  return total;
}

export function normalizeContextRole(role: string): string {
  const normalized = role.trim().toLowerCase();
  return normalized.length > 0 ? normalized : "unknown";
}

export function getContextRoleLabel(role: string): string {
  if (role === "system") return "System prompt";
  if (role === "user") return "User messages";
  if (role === "assistant") return "Assistant messages";
  if (role === "tool") return "Tool messages";
  return `${role} messages`;
}

export function getContextRoleBadgeClass(role: string): string {
  if (role === "system") {
    return "border-sky-200 bg-sky-50 text-sky-700";
  }
  if (role === "user") {
    return "border-emerald-200 bg-emerald-50 text-emerald-700";
  }
  if (role === "assistant") {
    return "border-indigo-200 bg-indigo-50 text-indigo-700";
  }
  if (role === "tool") {
    return "border-amber-200 bg-amber-50 text-amber-700";
  }
  return "border-zinc-200 bg-zinc-50 text-zinc-700";
}

export function getContextRoleBarClass(role: string): string {
  if (role === "system") return "bg-sky-500";
  if (role === "user") return "bg-emerald-500";
  if (role === "assistant") return "bg-indigo-500";
  if (role === "tool") return "bg-amber-500";
  return "bg-zinc-500";
}

function normalizeRoleBucketsToTotal(
  rawBuckets: Array<{ role: string; tokens: number; messages: number }>,
  targetTotal: number,
): ContextRoleTokenBucket[] {
  const totalRaw = rawBuckets.reduce((sum, bucket) => sum + bucket.tokens, 0);
  if (totalRaw <= 0 || targetTotal <= 0) {
    return rawBuckets.map((bucket) => ({
      role: bucket.role,
      label: getContextRoleLabel(bucket.role),
      messages: bucket.messages,
      tokens: 0,
    }));
  }
  const scaled = rawBuckets.map((bucket) => ({
    role: bucket.role,
    messages: bucket.messages,
    exact: (bucket.tokens / totalRaw) * targetTotal,
  }));
  const floors = new Map(scaled.map((entry) => [entry.role, Math.floor(entry.exact)]));
  let remainder = targetTotal - [...floors.values()].reduce((sum, value) => sum + value, 0);
  if (remainder > 0) {
    const byFractionDesc = [...scaled].sort(
      (a, b) => (b.exact - Math.floor(b.exact)) - (a.exact - Math.floor(a.exact)),
    );
    let index = 0;
    while (remainder > 0) {
      const current = byFractionDesc[index % byFractionDesc.length];
      floors.set(current.role, (floors.get(current.role) ?? 0) + 1);
      remainder -= 1;
      index += 1;
    }
  }
  const buckets = rawBuckets.map((bucket) => ({
    role: bucket.role,
    label: getContextRoleLabel(bucket.role),
    messages: bucket.messages,
    tokens: floors.get(bucket.role) ?? 0,
  }));
  buckets.sort((left, right) => {
    const leftIndex = CONTEXT_ROLE_ORDER.indexOf(left.role as (typeof CONTEXT_ROLE_ORDER)[number]);
    const rightIndex = CONTEXT_ROLE_ORDER.indexOf(right.role as (typeof CONTEXT_ROLE_ORDER)[number]);
    const normalizedLeftIndex = leftIndex >= 0 ? leftIndex : Number.MAX_SAFE_INTEGER;
    const normalizedRightIndex = rightIndex >= 0 ? rightIndex : Number.MAX_SAFE_INTEGER;
    if (normalizedLeftIndex !== normalizedRightIndex) {
      return normalizedLeftIndex - normalizedRightIndex;
    }
    if (left.tokens !== right.tokens) {
      return right.tokens - left.tokens;
    }
    return left.role.localeCompare(right.role);
  });
  return buckets;
}

export function buildContextRoleTokenBreakdown(
  snapshot: SessionContextDebugSnapshot,
  promptTokens?: number,
): ContextRoleTokenBreakdown {
  const assembledMessages =
    snapshot.windowSnapshot?.assembledMessages ?? snapshot.assembledMessages;
  const roleBuckets = new Map<string, { role: string; tokens: number; messages: number }>();
  for (const message of assembledMessages) {
    const tokens = estimateMessageTokens(message);
    if (tokens <= 0) {
      continue;
    }
    const role = normalizeContextRole(message.role);
    const bucket = roleBuckets.get(role) ?? { role, tokens: 0, messages: 0 };
    bucket.tokens += tokens;
    bucket.messages += 1;
    roleBuckets.set(role, bucket);
  }
  const rawBuckets = [...roleBuckets.values()];
  const targetTotal =
    typeof promptTokens === "number" && promptTokens > 0
      ? promptTokens
      : rawBuckets.reduce((sum, bucket) => sum + bucket.tokens, 0);
  const buckets = normalizeRoleBucketsToTotal(rawBuckets, targetTotal);
  return {
    buckets,
    total: buckets.reduce((sum, bucket) => sum + bucket.tokens, 0),
  };
}

// ── Tool result parsing ──────────────────────────────────

const LEGACY_TERMINAL_ERROR_PREFIX = 'Error: terminal command failed: ';

interface NormalizedToolResult {
  /** The unwrapped value: the inner terminal JSON, or raw text, or undefined. */
  value: unknown;
  /** True if this came from a `ToolResult::Error` (transport-level tool failure). */
  isToolError: boolean;
  /** Raw error message when `isToolError`, or undefined. */
  errorMessage?: string;
}

/**
 * Unwrap the `ToolResult` envelope (`{ kind: 'json'|'text'|..., ... }`) one
 * level, including legacy `ToolResult::Error` payloads whose message embeds
 * a stringified terminal JSON (pre-P1 rows in the history DB). The returned
 * `value` is the inner payload the rest of the pipeline should reason about.
 */
function normalizeToolResultEnvelope(rawResult: unknown): NormalizedToolResult {
  if (!rawResult || typeof rawResult !== 'object' || Array.isArray(rawResult)) {
    return { value: rawResult, isToolError: false };
  }
  const record = rawResult as Record<string, unknown>;

  if ('Ok' in record || 'ok' in record || 'Err' in record || 'err' in record) {
    const okValue = record.Ok ?? record.ok;
    const errValue = record.Err ?? record.err;
    if (okValue !== undefined) {
      return normalizeToolResultEnvelope(okValue);
    }
    if (errValue !== undefined) {
      const inner = normalizeToolResultEnvelope(errValue);
      return {
        value: inner.value,
        isToolError: true,
        errorMessage: inner.errorMessage,
      };
    }
  }

  const kind =
    typeof record.kind === 'string' ? record.kind.trim().toLowerCase() : '';

  if (kind === 'json' && record.value !== undefined) {
    return { value: record.value, isToolError: false };
  }
  if (kind === 'text' && typeof record.text === 'string') {
    return { value: record.text, isToolError: false };
  }
  if (kind === 'tsv' && typeof record.text === 'string') {
    return { value: record.text, isToolError: false };
  }
  if (kind === 'error' && typeof record.message === 'string') {
    const message = record.message;
    if (message.startsWith(LEGACY_TERMINAL_ERROR_PREFIX)) {
      const tail = message.slice(LEGACY_TERMINAL_ERROR_PREFIX.length).trim();
      if (tail.startsWith('{')) {
        try {
          return {
            value: JSON.parse(tail),
            isToolError: true,
            errorMessage: message,
          };
        } catch {
          // fall through to raw message
        }
      }
    }
    return { value: message, isToolError: true, errorMessage: message };
  }
  if (kind === 'done') {
    return { value: undefined, isToolError: false };
  }

  return { value: rawResult, isToolError: false };
}

export function parseToolResultPayload(rawResult: unknown): ParsedToolResult {
  const { value, isToolError, errorMessage } = normalizeToolResultEnvelope(rawResult);

  const parsed = (() => {
    if (typeof value !== 'string') {
      return value;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      return '';
    }
    const jsonLike =
      (trimmed.startsWith('{') && trimmed.endsWith('}')) ||
      (trimmed.startsWith('[') && trimmed.endsWith(']'));
    if (!jsonLike) {
      return trimmed;
    }
    try {
      return JSON.parse(trimmed);
    } catch {
      return trimmed;
    }
  })();

  const parsedObject =
    parsed && typeof parsed === 'object' && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : undefined;
  const hasTerminalShape =
    !!parsedObject &&
    (Object.prototype.hasOwnProperty.call(parsedObject, 'stdout') ||
      Object.prototype.hasOwnProperty.call(parsedObject, 'stderr') ||
      Object.prototype.hasOwnProperty.call(parsedObject, 'exit_code'));
  const exitCode =
    parsedObject && typeof parsedObject.exit_code === 'number'
      ? (parsedObject.exit_code as number)
      : undefined;
  const status =
    parsedObject && typeof parsedObject.status === 'string'
      ? (parsedObject.status as string).trim().toLowerCase()
      : '';

  const serializedParsed =
    parsed && typeof parsed === 'object'
      ? (() => {
          try {
            return JSON.stringify(parsed);
          } catch {
            return undefined;
          }
        })()
      : undefined;

  // For terminal-shaped JSON: always serialize back so downstream
  // `parseCommandOutput` can extract stdout/stderr/exit_code/task_id.
  const output = hasTerminalShape
    ? serializedParsed
    : typeof parsed === 'string' && parsed.trim().length > 0
      ? parsed
      : typeof serializedParsed === 'string' && serializedParsed.trim().length > 0
        ? serializedParsed
        : errorMessage;

  const stoppedByStatus = status === 'stopped' || status === 'cancelled';
  const failedByExitCode =
    typeof exitCode === 'number' && exitCode !== 0 && !stoppedByStatus;
  const failedByStatus = status === 'failed';
  const nextStatus: ParsedToolResult['status'] = stoppedByStatus
    ? 'stopped'
    : isToolError || failedByExitCode || failedByStatus
      ? 'failed'
      : 'done';

  const taskId =
    parsedObject && typeof parsedObject.task_id === 'string' && parsedObject.task_id.length > 0
      ? (parsedObject.task_id as string)
      : undefined;

  return {
    output,
    status: nextStatus,
    taskId,
  };
}

// ── Timeline segment helpers ─────────────────────────────

export function settleTimelineSegments(
  segments: TimelineSegment[] | undefined,
  settledAt: number,
  interruptedStatus: "failed" | "stopped" = "failed",
): TimelineSegment[] {
  return (segments ?? []).map((segment) => {
    if (segment.kind === "thinking") {
      return {
        ...segment,
        isRunning: false,
        endedAt: segment.endedAt ?? settledAt,
      };
    }

    if (segment.kind === "tool") {
      return {
        ...segment,
        tool:
          segment.tool.status === "running"
            ? {
                ...segment.tool,
                status: interruptedStatus,
                endedAt: segment.tool.endedAt ?? settledAt,
              }
            : segment.tool,
      };
    }

    return {
      ...segment,
      endedAt: segment.endedAt ?? settledAt,
      commands: segment.commands.map((command) =>
        command.status === "running"
          ? {
              ...command,
              status: interruptedStatus,
              endedAt: command.endedAt ?? settledAt,
            }
          : command,
      ),
    };
  });
}

export function summarizeTimelineSegments(segments: TimelineSegment[]): {
  thinking: string;
  terminalExecutions: TerminalExecution[];
} {
  const thinkingChunks: string[] = [];
  const terminalExecutions: TerminalExecution[] = [];

  for (const segment of segments) {
    if (segment.kind === "thinking") {
      thinkingChunks.push(segment.content);
      continue;
    }
    if (segment.kind === "tool") {
      continue;
    }
    terminalExecutions.push(...segment.commands.map((command) => ({ ...command })));
  }

  return {
    thinking: thinkingChunks.join("\n\n").trim(),
    terminalExecutions,
  };
}
