import { Suspense, lazy, memo, useState, useMemo, useEffect, useRef } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { motion, AnimatePresence } from 'framer-motion';
import { cn } from '@/lib/utils';
import { buildSessionOutputPreviewUrl, fetchSessionTasks, saveSessionTasks } from '@/lib/api';
import type { ArtifactPreviewTarget, Message, SessionPlanState } from '@/types';
import { useStore } from '@/lib/store';
import { extractArtifactPreviewTargets, stripArtifactPreviewTags } from './chatUtils';
import { AttachmentCard } from './AttachmentCard';
import ReactMarkdown from 'react-markdown';
import { Copy, Check, FileText, ChevronRight, RotateCcw, Download, Terminal, Globe, Search, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';

const TASKS_TAG_PATTERN = /<tasks\s*\/>/i;
const LazySyntaxCodeBlock = lazy(() => import('./SyntaxCodeBlock'));
const LazyFlowMarkdown = lazy(() =>
  import('./StreamingText').then((module) => ({ default: module.FlowMarkdown })),
);

function stripTasksTag(content: string): string {
  return content.replace(/<tasks\s*\/>/gi, "").trim();
}

const MemoMarkdown = memo(function MemoMarkdown({
  content,
  components,
}: {
  content: string;
  components: any;
}) {
  return <ReactMarkdown components={components}>{content}</ReactMarkdown>;
});

async function copyWithRichText(markdown: string) {
  const plain = markdown ?? '';

  const html = renderToStaticMarkup(
    <ReactMarkdown>{plain}</ReactMarkdown>,
  );

  if (typeof ClipboardItem !== 'undefined' && navigator.clipboard?.write) {
    const item = new ClipboardItem({
      'text/plain': new Blob([plain], { type: 'text/plain' }),
      'text/html': new Blob([html], { type: 'text/html' }),
    });
    await navigator.clipboard.write([item]);
    return;
  }

  await navigator.clipboard.writeText(plain);
}

function CopyButton({ text, className }: { text: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button variant="ghost" size="icon" className={className}
      onClick={async () => {
        await copyWithRichText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 2000);
      }}>
      {copied ? <Check className="h-3 w-3 text-green-400" /> : <Copy className="h-3 w-3" />}
    </Button>
  );
}

function detectCodeLanguage(text: string): 'shell' | 'json' | 'text' {
  const trimmed = text.trim();
  if (!trimmed) return 'text';
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try {
      JSON.parse(trimmed);
      return 'json';
    } catch {
      // fall through
    }
  }
  if (/(\$\s+|bash|zsh|sh\s|pwsh|powershell)/i.test(trimmed)) return 'shell';
  return 'text';
}

function formatCodeText(text: string, language: 'shell' | 'json' | 'text'): string {
  if (language !== 'json') return text;
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function formatDurationMs(durationMs: number): string {
  const safeDuration = Math.max(0, Math.round(durationMs));
  const totalSeconds = Math.max(1, Math.floor(safeDuration / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}m ${seconds.toString().padStart(2, '0')}s`;
}

function buildRunningTimelineLabel(
  item: TimelineDisplayItem,
): string {
  if (item.type === 'thinking') {
    return 'Thinking';
  }
  if (item.type === 'command') {
    return item.description.trim();
  }
  if (item.type === 'websearch') {
    return 'Searching the web';
  }
  return 'Fetching page';
}

function buildCollapsedTimelineLabel(item: TimelineDisplayItem): string {
  if (item.type === 'thinking') {
    return 'Thinking';
  }
  if (item.type === 'command') {
    return item.description.trim();
  }
  if (item.type === 'websearch') {
    return 'Web search';
  }
  return 'Web fetch';
}

interface ParsedCommandImage {
  src: string;
  mediaType: string;
  source: 'inline' | 'stored';
  storagePath?: string;
}

interface ParsedCommandOutput {
  text?: string;
  stdoutText?: string;
  stderrText?: string;
  exitCode?: number;
  images: ParsedCommandImage[];
  storagePaths: string[];
}

function parseDataUrlImage(value: string): ParsedCommandImage | null {
  const trimmed = value.trim();
  if (!trimmed.startsWith('data:image/')) {
    return null;
  }
  const match = /^data:(image\/[a-z0-9.+-]+);base64,([a-z0-9+/=\s]+)$/i.exec(trimmed);
  if (!match) {
    return null;
  }
  const mediaType = match[1].toLowerCase();
  const encoded = match[2].replace(/\s+/g, '');
  if (!encoded) {
    return null;
  }
  return {
    src: `data:${mediaType};base64,${encoded}`,
    mediaType,
    source: 'inline',
  };
}

function isLikelyBinaryTextPayload(text: string): boolean {
  if (!text) {
    return false;
  }
  const sample = text.slice(0, 4096);
  const lowercaseSample = sample.toLowerCase();
  if (
    sample.length >= 64 &&
    (
      lowercaseSample.includes('jfif') ||
      lowercaseSample.includes('exif') ||
      lowercaseSample.includes('ihdr') ||
      lowercaseSample.includes('png') ||
      lowercaseSample.includes('webp') ||
      lowercaseSample.includes('riff')
    )
  ) {
    return true;
  }
  let suspiciousUnits = 0;
  let symbolicUnits = 0;
  let languageLikeUnits = 0;
  for (const char of sample) {
    const code = char.codePointAt(0) ?? 0;
    if (code === 0xfffd) {
      suspiciousUnits += 2;
      continue;
    }
    const isAsciiControl = code < 0x20 && code !== 0x09 && code !== 0x0a && code !== 0x0d;
    if (isAsciiControl || code === 0x7f || code === 0x00) {
      suspiciousUnits += 1;
    }
    if (/[\[\]{}<>\\|`~^]/.test(char)) {
      symbolicUnits += 1;
    }
    if (
      (code >= 0x30 && code <= 0x39) ||
      (code >= 0x41 && code <= 0x5a) ||
      (code >= 0x61 && code <= 0x7a) ||
      (code >= 0x4e00 && code <= 0x9fff)
    ) {
      languageLikeUnits += 1;
    }
  }
  if (suspiciousUnits > 0 && suspiciousUnits / Math.max(1, sample.length) > 0.02) {
    return true;
  }
  if (sample.length >= 320) {
    const symbolicRatio = symbolicUnits / sample.length;
    const languageLikeRatio = languageLikeUnits / sample.length;
    if (symbolicRatio > 0.16 && languageLikeRatio < 0.52) {
      return true;
    }
  }
  return false;
}

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

function unwrapStructuredOutputNode(value: unknown): unknown {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return value;
  }
  const node = value as Record<string, unknown>;
  const kind =
    typeof node.kind === 'string' ? node.kind.trim().toLowerCase() : '';
  if (kind === 'json' && node.value !== undefined) {
    return node.value;
  }
  if ((kind === 'text' || kind === 'error') && node.value !== undefined) {
    return node.value;
  }
  return value;
}

function parseOutputEntry(
  entry: unknown,
  parsed: ParsedCommandOutput,
  textChunks: string[],
  sourcePath?: string,
) {
  const unwrappedEntry = unwrapStructuredOutputNode(entry);
  if (unwrappedEntry !== entry) {
    parseOutputEntry(unwrappedEntry, parsed, textChunks, sourcePath);
    return;
  }
  if (entry === null || entry === undefined) {
    return;
  }
  if (typeof entry === 'string') {
    const inlineImage = parseDataUrlImage(entry);
    if (inlineImage) {
      parsed.images.push(inlineImage);
      return;
    }
    if (isLikelyBinaryTextPayload(entry)) {
      return;
    }
    if (entry.trim().length > 0) {
      textChunks.push(entry);
    }
    return;
  }
  if (Array.isArray(entry)) {
    for (const item of entry) {
      parseOutputEntry(item, parsed, textChunks, sourcePath);
    }
    return;
  }
  if (typeof entry !== 'object') {
    return;
  }
  const node = entry as Record<string, unknown>;
  const pathCandidates = [
    typeof node.url === 'string' ? node.url.trim() : '',
    typeof node.path === 'string' ? node.path.trim() : '',
    typeof node.storage_path === 'string' ? node.storage_path.trim() : '',
    typeof node.storagePath === 'string' ? node.storagePath.trim() : '',
    typeof node.output_path === 'string' ? node.output_path.trim() : '',
  ].filter((value) => value.length > 0);
  const effectiveSourcePath = pathCandidates[0] || sourcePath;
  if (pathCandidates.length > 0) {
    parsed.storagePaths.push(...pathCandidates);
  }
  const content = node.content;
  const payload =
    content && typeof content === 'object' && !Array.isArray(content)
      ? (unwrapStructuredOutputNode(content) as Record<string, unknown>)
      : node;
  const kindSource =
    typeof payload.type === 'string'
      ? payload.type
      : typeof payload.kind === 'string'
        ? payload.kind
        : '';
  const kind = kindSource.trim().toLowerCase();
  if (kind === 'text') {
    const textCandidate =
      typeof payload.text === 'string'
        ? payload.text
        : typeof payload.value === 'string'
          ? payload.value
          : '';
    const text = textCandidate;
    if (text.trim().length > 0) {
      if (isLikelyBinaryTextPayload(text)) {
        return;
      }
      textChunks.push(text);
    }
    return;
  }
  if (kind === 'image') {
    const encoded =
      typeof payload.data === 'string'
        ? payload.data.trim()
        : typeof payload.value === 'string'
          ? payload.value.trim()
          : '';
    if (!encoded) {
      return;
    }
    const mediaType =
      typeof payload.media_type === 'string' && payload.media_type.trim().length > 0
        ? payload.media_type.trim()
        : 'image/png';
    parsed.images.push({
      src: `data:${mediaType};base64,${encoded}`,
      mediaType,
      source: 'inline',
      storagePath: effectiveSourcePath || undefined,
    });
    return;
  }

  if (kind === 'image_url') {
    const imageUrl =
      typeof payload.image_url === 'string'
        ? payload.image_url.trim()
        : payload.image_url && typeof payload.image_url === 'object'
          ? typeof (payload.image_url as Record<string, unknown>).url === 'string'
            ? ((payload.image_url as Record<string, unknown>).url as string).trim()
            : ''
          : typeof payload.url === 'string'
            ? payload.url.trim()
            : '';
    if (!imageUrl) {
      return;
    }
    const inlineImage = parseDataUrlImage(imageUrl);
    if (inlineImage) {
      parsed.images.push(inlineImage);
      return;
    }
    parsed.images.push({
      src: imageUrl,
      mediaType: 'image/*',
      source: 'stored',
      storagePath: effectiveSourcePath || undefined,
    });
    return;
  }

  if (Object.prototype.hasOwnProperty.call(node, 'stdout') || Object.prototype.hasOwnProperty.call(node, 'stderr')) {
    parseOutputEntry(node.stdout, parsed, textChunks, effectiveSourcePath);
    parseOutputEntry(node.stderr, parsed, textChunks, effectiveSourcePath);
    return;
  }

  if (Object.prototype.hasOwnProperty.call(node, 'content')) {
    parseOutputEntry(node.content, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'contents')) {
    parseOutputEntry(node.contents, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'output')) {
    parseOutputEntry(node.output, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'outputs')) {
    parseOutputEntry(node.outputs, parsed, textChunks, effectiveSourcePath);
  }
  if (Object.prototype.hasOwnProperty.call(node, 'items')) {
    parseOutputEntry(node.items, parsed, textChunks, effectiveSourcePath);
  }
}

function parseCommandOutput(output?: string): ParsedCommandOutput {
  if (!output || output.trim().length === 0) {
    return {
      images: [],
      storagePaths: [],
    };
  }

  const buildParsedOutput = (entry: unknown, sourcePath?: string): ParsedCommandOutput => {
    const parsed: ParsedCommandOutput = {
      images: [],
      storagePaths: [],
    };
    const textChunks: string[] = [];
    parseOutputEntry(entry, parsed, textChunks, sourcePath);
    if (textChunks.length === 0) {
      return parsed;
    }
    const merged = textChunks.join('\n\n').trim();
    if (merged.length > 0) {
      parsed.text = formatCodeText(merged, detectCodeLanguage(merged));
    }
    parsed.storagePaths = [...new Set(parsed.storagePaths)];
    return parsed;
  };

  const interpreted = unwrapStructuredOutputNode(parseJsonLike(output));
  if (
    interpreted &&
    typeof interpreted === 'object' &&
    !Array.isArray(interpreted)
  ) {
    const record = interpreted as Record<string, unknown>;
    const hasTerminalShape =
      Object.prototype.hasOwnProperty.call(record, 'stdout') ||
      Object.prototype.hasOwnProperty.call(record, 'stderr') ||
      Object.prototype.hasOwnProperty.call(record, 'exit_code');
    if (hasTerminalShape) {
      const stdout = buildParsedOutput(record.stdout);
      const stderr = buildParsedOutput(record.stderr);
      return {
        text: stdout.text ?? stderr.text,
        stdoutText: stdout.text,
        stderrText: stderr.text,
        exitCode:
          typeof record.exit_code === 'number' ? record.exit_code : undefined,
        images: [...stdout.images, ...stderr.images],
        storagePaths: [...new Set([...stdout.storagePaths, ...stderr.storagePaths])],
      };
    }
  }

  const parsed = buildParsedOutput(interpreted);
  if (
    !parsed.text &&
    parsed.images.length === 0 &&
    parsed.storagePaths.length === 0
  ) {
    if (isLikelyBinaryTextPayload(output)) {
      return parsed;
    }
    return {
      ...parsed,
      text: formatCodeText(output, detectCodeLanguage(output)),
    };
  }
  return parsed;
}

interface WebSearchResultEntry {
  title: string;
  url: string;
  snippet: string;
  host: string;
}

interface ThinkingTimelineDisplayItem {
  id: string;
  type: 'thinking';
  summary: string;
  detail: string;
  isRunning: boolean;
  isFailed: boolean;
  startedAt?: number;
  endedAt?: number;
}

interface CommandTimelineDisplayItem {
  id: string;
  type: 'command';
  summary: string;
  description: string;
  commandInput: string;
  parsedOutput: ParsedCommandOutput;
  outputText: string;
  isRunning: boolean;
  isFailed: boolean;
  startedAt?: number;
  endedAt?: number;
}

interface WebSearchTimelineDisplayItem {
  id: string;
  type: 'websearch';
  summary: string;
  query: string;
  requestRaw?: string;
  responseRaw?: string;
  results: WebSearchResultEntry[];
  isRunning: boolean;
  isFailed: boolean;
  startedAt?: number;
  endedAt?: number;
}

interface WebFetchTimelineDisplayItem {
  id: string;
  type: 'webfetch';
  summary: string;
  url: string;
  requestRaw?: string;
  responseRaw?: string;
  isRunning: boolean;
  isFailed: boolean;
  startedAt?: number;
  endedAt?: number;
}

type TimelineDisplayItem =
  | ThinkingTimelineDisplayItem
  | CommandTimelineDisplayItem
  | WebSearchTimelineDisplayItem
  | WebFetchTimelineDisplayItem;

function toSingleLineSummary(text: string, limit = 92): string {
  const normalized = text.replace(/\s+/g, ' ').trim();
  if (!normalized) {
    return '';
  }
  if (normalized.length <= limit) {
    return normalized;
  }
  return `${normalized.slice(0, Math.max(0, limit - 1)).trimEnd()}…`;
}

function normalizeToolName(name: string): string {
  return name.trim().toLowerCase();
}

function isWebSearchToolName(name: string): boolean {
  return (
    name === 'websearch' ||
    name.endsWith('.websearch') ||
    name.endsWith('/websearch')
  );
}

function isWebFetchToolName(name: string): boolean {
  return (
    name === 'webfetch' ||
    name.endsWith('.webfetch') ||
    name.endsWith('/webfetch')
  );
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

function parseWebSearchQuery(rawRequest: string | undefined): string {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return '';
  }
  const query = parsed.query;
  return typeof query === 'string' ? query.trim() : '';
}

function parseWebFetchUrl(rawRequest: string | undefined): string {
  const parsed = parseJsonRecord(rawRequest);
  if (!parsed) {
    return '';
  }
  const urlCandidate = parsed.url ?? parsed.uri ?? parsed.link;
  return typeof urlCandidate === 'string' ? urlCandidate.trim() : '';
}

function extractHost(urlValue: string): string {
  try {
    return new URL(urlValue).hostname;
  } catch {
    return urlValue;
  }
}

function parseWebSearchResults(rawResponse: string | undefined): WebSearchResultEntry[] {
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

function StoredOutputImage({ src, alt }: { src: string; alt: string }) {
  const [failed, setFailed] = useState(false);
  if (failed) {
    return null;
  }
  return (
    <img
      src={src}
      alt={alt}
      className="h-auto max-h-[420px] w-full max-w-full rounded-lg border border-zinc-800 bg-zinc-950/80 object-contain"
      loading="lazy"
      onError={() => setFailed(true)}
    />
  );
}

function CommandCodePanel({
  label,
  code,
  language,
}: {
  label: string;
  code: string;
  language: 'shell' | 'json' | 'text';
}) {
  return (
    <div className="overflow-hidden rounded-md border border-zinc-800 bg-zinc-950/80">
      <div className="border-b border-zinc-800 px-2.5 py-1.5 text-[10px] font-medium uppercase tracking-[0.12em] text-zinc-500">
        {label}
      </div>
      <Suspense
        fallback={
          <pre className="max-h-[260px] overflow-y-auto whitespace-pre-wrap break-all px-3 py-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
            {code}
          </pre>
        }
      >
        <div className="max-h-[260px] overflow-y-auto scrollbar-none">
          <LazySyntaxCodeBlock language={language} code={code} />
        </div>
      </Suspense>
    </div>
  );
}

function makeComponents(isUser: boolean, isStreaming: boolean) {
  return {
    code({ className, children, ...props }: any) {
      const lang = /language-(\w+)/.exec(className || '')?.[1];
      const code = String(children).replace(/\n$/, '');
      return lang ? (
        <div className="relative group/code my-3 rounded-lg overflow-hidden border border-border/30">
          <div className="flex items-center justify-between px-4 py-1.5 bg-zinc-800 text-zinc-400 text-xs">
            <span>{lang}</span>
            {!isStreaming && <CopyButton text={code} className="h-5 w-5 text-zinc-400 hover:text-zinc-100 hover:bg-zinc-700/50" />}
          </div>
          <Suspense
            fallback={
              <pre className="m-0 overflow-x-auto bg-zinc-950 px-4 py-3 text-[0.8125rem] text-zinc-100">
                <code>{code}</code>
              </pre>
            }
          >
            <LazySyntaxCodeBlock language={lang} code={code} />
          </Suspense>
        </div>
      ) : (
        <code {...props} className={cn('rounded px-1 py-0.5 font-mono text-sm', isUser ? 'bg-black/10 dark:bg-white/10' : 'bg-muted', className)}>{children}</code>
      );
    },
    p: ({ children }: any) => <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>,
    ul: ({ children }: any) => <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>,
    ol: ({ children }: any) => <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>,
  };
}

export function MessageItem({
  message,
  sessionId,
  onRetry,
  canRetry,
  onOpenPreview,
}: {
  message: Message;
  sessionId?: string;
  onRetry?: () => void;
  canRetry?: boolean;
  onOpenPreview?: (target: ArtifactPreviewTarget) => void;
}) {
  const isUser = message.role === 'user';
  const isStreaming = !!message.isStreaming;
  const terminalExecutions = message.terminalExecutions || [];
  const [expandedTimelineItemIds, setExpandedTimelineItemIds] = useState<Record<string, boolean>>({});
  const timelineExpansionOverridesRef = useRef<Record<string, boolean>>({});
  const [showTyping, setShowTyping] = useState(false);
  const [tasksContent, setTasksContent] = useState("");
  const [tasksDraft, setTasksDraft] = useState("");
  const [tasksLoading, setTasksLoading] = useState(false);
  const [tasksSaving, setTasksSaving] = useState(false);
  const [tasksEditing, setTasksEditing] = useState(false);
  const [tasksError, setTasksError] = useState<string | null>(null);
  const [tasksPlanState, setTasksPlanState] = useState<SessionPlanState>('disabled');
  const setSessionPlanState = useStore((state) => state.setSessionPlanState);
  const components = useMemo(
    () => makeComponents(isUser, isStreaming),
    [isUser, isStreaming]
  );
  const showsTasksTag = useMemo(
    () => TASKS_TAG_PATTERN.test(message.content || ""),
    [message.content],
  );

  const previewTargets = useMemo(
    () => extractArtifactPreviewTargets(message.content || ''),
    [message.content],
  );
  const displayContent = useMemo(
    () => stripTasksTag(stripArtifactPreviewTags(message.content || '')),
    [message.content],
  );
  const timelineItems = useMemo<TimelineDisplayItem[]>(() => {
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
      for (const command of terminalExecutions) {
        const parsedOutput = parseCommandOutput(command.output);
        const outputLang = detectCodeLanguage(parsedOutput.text || '');
        items.push({
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
          startedAt: command.startedAt ?? fallbackStartedAt,
          endedAt: command.endedAt,
        });
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
          const parsedOutput = parseCommandOutput(command.output);
          const outputLang = detectCodeLanguage(parsedOutput.text || '');
          items.push({
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
            startedAt: command.startedAt ?? segment.startedAt,
            endedAt: command.endedAt ?? segment.endedAt,
          });
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
          startedAt: segment.tool.startedAt,
          endedAt: segment.tool.endedAt,
        });
      }
    }
    return items;
  }, [
    message.terminalExecutions,
    message.id,
    message.isThinking,
    message.thinking,
    message.timelineSegments,
    message.timestamp,
    terminalExecutions,
  ]);
  const hasImageLikeTerminalOutput = useMemo(
    () => timelineItems.some((item) => item.type === 'command' && (item.parsedOutput.images.length > 0 || item.parsedOutput.storagePaths.length > 0)),
    [timelineItems],
  );
  const suppressBinaryContent = useMemo(() => {
    if (isUser || timelineItems.length === 0) {
      return false;
    }
    const normalized = displayContent.trim();
    if (!normalized) {
      return false;
    }
    if (!isLikelyBinaryTextPayload(normalized)) {
      return false;
    }
    if (hasImageLikeTerminalOutput) {
      return true;
    }
    return normalized.length >= 256;
  }, [displayContent, hasImageLikeTerminalOutput, isUser, timelineItems.length]);
  const safeDisplayContent = suppressBinaryContent ? '' : displayContent;

  useEffect(() => {
    setExpandedTimelineItemIds({});
    timelineExpansionOverridesRef.current = {};
  }, [message.id]);

  useEffect(() => {
    if (!isStreaming || message.content.length > 0 || !!message.error) {
      setShowTyping(false);
      return;
    }

    const timer = window.setTimeout(() => setShowTyping(true), 1000);
    return () => window.clearTimeout(timer);
  }, [isStreaming, message.content, message.id, message.error]);

  useEffect(() => {
    if (!showsTasksTag || !sessionId) {
      return;
    }
    let cancelled = false;
    setTasksLoading(true);
    setTasksError(null);
    void fetchSessionTasks(sessionId)
      .then((payload) => {
        if (cancelled) return;
        setTasksContent(payload.content);
        setTasksDraft(payload.content);
        setTasksPlanState(payload.planState);
      })
      .catch((error) => {
        if (cancelled) return;
        setTasksError(error instanceof Error ? error.message : String(error));
      })
      .finally(() => {
        if (!cancelled) {
          setTasksLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, showsTasksTag]);

  const persistTasks = async (
    content: string,
    planState: SessionPlanState,
    nextEditing: boolean,
  ) => {
    if (!sessionId) {
      return;
    }
    setTasksSaving(true);
    setTasksError(null);
    try {
      const payload = await saveSessionTasks(sessionId, {
        content,
        planState,
      });
      setTasksContent(payload.content);
      setTasksDraft(payload.content);
      setTasksPlanState(payload.planState);
      setTasksEditing(nextEditing);
      setSessionPlanState(sessionId, payload.planState);
    } catch (error) {
      setTasksError(error instanceof Error ? error.message : String(error));
    } finally {
      setTasksSaving(false);
    }
  };

  useEffect(() => {
    if (isUser || !onOpenPreview || previewTargets.length === 0) return;
    onOpenPreview(previewTargets[previewTargets.length - 1]);
  }, [isUser, onOpenPreview, previewTargets]);

  useEffect(() => {
    const visibleTimelineItemIds = new Set(timelineItems.map((item) => item.id));
    setExpandedTimelineItemIds((previous) => {
      const nextEntries = Object.entries(previous).filter(([itemId]) =>
        visibleTimelineItemIds.has(itemId),
      );
      const next = Object.fromEntries(nextEntries);
      for (const item of timelineItems) {
        if (timelineExpansionOverridesRef.current[item.id] !== undefined) {
          continue;
        }
        next[item.id] = item.isRunning;
      }
      return next;
    });
    for (const itemId of Object.keys(timelineExpansionOverridesRef.current)) {
      if (!visibleTimelineItemIds.has(itemId)) {
        delete timelineExpansionOverridesRef.current[itemId];
      }
    }
  }, [timelineItems]);

  const nowMs = Date.now();

  return (
    <motion.div
      initial={{ opacity: 0, y: 16, scale: 0.97 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      transition={{ type: 'spring', stiffness: 500, damping: 30 }}
      className={cn('flex w-full gap-2 p-2 group/message', isUser ? 'flex-row-reverse' : 'flex-row')}>
      <div className={cn('flex flex-col relative', isUser ? 'max-w-[75%] items-end' : 'w-full max-w-[85%] items-start')}>
        <div className={cn(
          'prose prose-sm dark:prose-invert max-w-none break-words relative prose-pre:p-0 prose-pre:m-0 prose-pre:bg-transparent',
          isUser
            ? 'px-4 py-2.5 rounded-2xl bg-primary text-primary-foreground rounded-br-md'
            : 'py-1 text-foreground w-full'
        )}>
          {!isUser && timelineItems.length > 0 && !message.toolEvent && (
            <div className="mb-3 space-y-2 not-prose">
              {timelineItems.map((item, index) => {
                const isExpanded = expandedTimelineItemIds[item.id] === true;
                const startedAt = item.startedAt ?? Date.parse(message.timestamp);
                const normalizedStartedAt = Number.isFinite(startedAt) ? startedAt : nowMs;
                const endedAt = item.endedAt ?? (item.isRunning ? nowMs : normalizedStartedAt);
                const durationText = formatDurationMs(Math.max(0, endedAt - normalizedStartedAt));
                const rowSummaryText = item.isRunning
                  ? buildRunningTimelineLabel(item)
                  : buildCollapsedTimelineLabel(item);
                const storedPreviewItems =
                  item.type === 'command' && sessionId && item.parsedOutput.storagePaths.length > 0
                    ? item.parsedOutput.storagePaths.map((storagePath) => {
                        try {
                          return {
                            storagePath,
                            src: buildSessionOutputPreviewUrl(sessionId, storagePath),
                          };
                        } catch {
                          return {
                            storagePath,
                            src: '',
                          };
                        }
                      })
                    : [];

                return (
                  <div
                    key={item.id}
                    className={cn(
                      'overflow-hidden rounded-lg border',
                      item.isRunning
                        ? 'border-zinc-700/70 bg-zinc-900/72'
                        : 'border-zinc-800/80 bg-zinc-950/38',
                    )}
                  >
                    <button
                      type="button"
                      className="flex w-full items-center gap-2 px-2.5 py-2 text-left"
                      onClick={() =>
                        setExpandedTimelineItemIds((previous) => {
                          const nextExpanded = !isExpanded;
                          timelineExpansionOverridesRef.current[item.id] = nextExpanded;
                          return {
                            ...previous,
                            [item.id]: nextExpanded,
                          };
                        })
                      }
                    >
                      <span className="text-[11px] tabular-nums text-zinc-600">{index + 1}</span>
                      {item.type === 'command' && <Terminal className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                      {item.type === 'thinking' && <FileText className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                      {item.type === 'websearch' && <Search className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                      {item.type === 'webfetch' && <Globe className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                      <span
                        className={cn(
                          'min-w-0 flex-1 truncate text-[14px]',
                          'tracking-[0.01em]',
                          item.isRunning
                            ? 'timeline-running-label font-medium text-zinc-300'
                            : 'text-zinc-300',
                        )}
                      >
                        {rowSummaryText}
                      </span>
                      {item.isRunning ? (
                        <span className="text-[11px] text-zinc-500">{durationText}</span>
                      ) : item.isFailed ? (
                        <span className="text-[11px] text-red-300">failed</span>
                      ) : (
                        <span className="text-[11px] text-zinc-500">{durationText}</span>
                      )}
                      <motion.span
                        animate={{ rotate: isExpanded ? 90 : 0 }}
                        transition={{ duration: 0.12 }}
                        className="inline-flex h-4 w-4 items-center justify-center text-zinc-500"
                      >
                        <ChevronRight className="h-3.5 w-3.5" />
                      </motion.span>
                    </button>
                    <AnimatePresence initial={false}>
                      {isExpanded && (
                        <motion.div
                          initial={{ height: 0, opacity: 0 }}
                          animate={{ height: 'auto', opacity: 1 }}
                          exit={{ height: 0, opacity: 0 }}
                          transition={{ duration: 0.14 }}
                          className="overflow-hidden border-t border-zinc-800"
                        >
                          <div className="space-y-2 px-3 py-2 text-xs text-zinc-300">
                            {item.type === 'thinking' && (
                              <pre className="max-h-[240px] overflow-y-auto whitespace-pre-wrap rounded-md border border-zinc-800 bg-zinc-950/80 p-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
                                {item.detail}
                              </pre>
                            )}
                            {item.type === 'command' && (
                              <>
                                <CommandCodePanel
                                  label="command"
                                  code={item.commandInput}
                                  language="shell"
                                />
                                {typeof item.parsedOutput.exitCode === 'number' && (
                                  <div className="rounded-md border border-zinc-800 bg-zinc-950/70 px-2.5 py-1.5 text-[11px] text-zinc-400">
                                    exit code: {item.parsedOutput.exitCode}
                                  </div>
                                )}
                                {item.parsedOutput.stdoutText && (
                                  <CommandCodePanel
                                    label="stdout"
                                    code={item.parsedOutput.stdoutText}
                                    language={detectCodeLanguage(item.parsedOutput.stdoutText)}
                                  />
                                )}
                                {item.parsedOutput.stderrText && (
                                  <CommandCodePanel
                                    label="stderr"
                                    code={item.parsedOutput.stderrText}
                                    language={detectCodeLanguage(item.parsedOutput.stderrText)}
                                  />
                                )}
                                {!item.parsedOutput.stdoutText &&
                                  !item.parsedOutput.stderrText &&
                                  item.outputText && (
                                    <CommandCodePanel
                                      label="output"
                                      code={item.outputText}
                                      language={detectCodeLanguage(item.outputText)}
                                    />
                                  )}
                                {(item.parsedOutput.images.length > 0 || storedPreviewItems.length > 0) && (
                                  <div className="grid gap-2">
                                    {item.parsedOutput.images.map((image, imageIndex) => (
                                      <figure
                                        key={`${item.id}-inline-image-${imageIndex}`}
                                        className="space-y-1.5 rounded-md border border-zinc-800 bg-zinc-950/70 p-2"
                                      >
                                        <img
                                          src={image.src}
                                          alt={`Command output image (${image.mediaType})`}
                                          className="h-auto max-h-[360px] w-full max-w-full rounded object-contain"
                                          loading="lazy"
                                        />
                                        <figcaption className="break-all font-mono text-[10px] text-zinc-500">
                                          {image.mediaType}
                                        </figcaption>
                                      </figure>
                                    ))}
                                    {storedPreviewItems.map((previewItem, previewItemIndex) => (
                                      <div
                                        key={`${item.id}-stored-output-${previewItem.storagePath}-${previewItemIndex}`}
                                        className="space-y-1.5 rounded-md border border-zinc-800 bg-zinc-950/70 p-2"
                                      >
                                        {previewItem.src ? (
                                          <StoredOutputImage
                                            src={previewItem.src}
                                            alt={`Stored command output ${previewItem.storagePath}`}
                                          />
                                        ) : null}
                                        <div className="break-all font-mono text-[10px] text-zinc-500">
                                          {previewItem.storagePath}
                                        </div>
                                      </div>
                                    ))}
                                  </div>
                                )}
                              </>
                            )}
                            {item.type === 'websearch' && (
                              <div className="space-y-2">
                                {item.query && (
                                  <div className="rounded-md border border-zinc-800 bg-zinc-950/80 px-2 py-1.5 font-mono text-[11px] text-zinc-200">
                                    {item.query}
                                  </div>
                                )}
                                {item.results.length > 0 ? (
                                  <div className="space-y-1.5">
                                    {item.results.map((result, resultIndex) => (
                                      <a
                                        key={`${item.id}-result-${resultIndex}`}
                                        href={result.url}
                                        target="_blank"
                                        rel="noreferrer"
                                        className="block rounded-md border border-zinc-800 bg-zinc-950/70 px-2 py-1.5 hover:border-zinc-700"
                                      >
                                        <div className="line-clamp-1 text-[12px] text-zinc-100">{result.title}</div>
                                        <div className="text-[11px] text-zinc-500">{result.host}</div>
                                        {result.snippet ? (
                                          <div className="line-clamp-2 text-[11px] text-zinc-400">{result.snippet}</div>
                                        ) : null}
                                      </a>
                                    ))}
                                  </div>
                                ) : item.responseRaw ? (
                                  <pre className="max-h-[240px] overflow-y-auto whitespace-pre-wrap break-all rounded-md border border-zinc-800 bg-zinc-950/80 p-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
                                    {item.responseRaw}
                                  </pre>
                                ) : (
                                  <div className="text-[11px] text-zinc-500">No search results</div>
                                )}
                              </div>
                            )}
                            {item.type === 'webfetch' && (
                              <div className="space-y-2">
                                {item.url && (
                                  <div className="rounded-md border border-zinc-800 bg-zinc-950/80 px-2 py-1.5 font-mono text-[11px] text-zinc-200">
                                    {item.url}
                                  </div>
                                )}
                                {item.responseRaw ? (
                                  <pre className="max-h-[260px] overflow-y-auto whitespace-pre-wrap break-all rounded-md border border-zinc-800 bg-zinc-950/80 p-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
                                    {item.responseRaw}
                                  </pre>
                                ) : (
                                  <div className="text-[11px] text-zinc-500">No response body</div>
                                )}
                              </div>
                            )}
                          </div>
                        </motion.div>
                      )}
                    </AnimatePresence>
                  </div>
                );
              })}
            </div>
          )}

          {message.toolEvent && !isUser && (
            <div className="not-prose mb-2 rounded-md border border-border/40 bg-muted/20 px-3 py-2 text-xs text-muted-foreground font-mono whitespace-pre-wrap">
              {message.toolEvent.stage === 'start'
                ? `tool start: ${message.toolEvent.name ?? 'unknown'}`
                : `tool end: ${message.toolEvent.name ?? 'unknown'}`}
            </div>
          )}

          {message.error && !isUser && (
            <div className="not-prose mt-1 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <div>{message.error}</div>
              {onRetry && (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  className="mt-2 h-7 px-2 text-xs"
                  onClick={() => onRetry()}
                >
                  <RotateCcw className="mr-1 h-3 w-3" />
                  Retry
                </Button>
              )}
            </div>
          )}

          {suppressBinaryContent && (
            <div className="not-prose mb-2 rounded-xl border border-cyan-300/30 bg-cyan-500/10 px-3 py-2 text-xs text-cyan-100/95">
              Binary text output hidden. Open the activity timeline to view structured output and images.
            </div>
          )}

          {safeDisplayContent && (
            isUser ? (
              <MemoMarkdown content={safeDisplayContent} components={components} />
            ) : (
              <Suspense fallback={<MemoMarkdown content={safeDisplayContent} components={components} />}>
                <LazyFlowMarkdown content={safeDisplayContent} isStreaming={isStreaming} />
              </Suspense>
            )
          )}

          {!isUser && showsTasksTag && sessionId && (
            <div className="not-prose mt-3 rounded-xl border border-border/70 bg-card/80 px-3 py-3">
              <div className="mb-2 flex items-center justify-between gap-3">
                  <div>
                  <div className="text-sm font-medium text-foreground">tasks.md</div>
                  <div className="text-xs text-muted-foreground">Session task plan exposed via `&lt;tasks /&gt;`</div>
                  <div className="mt-1 text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
                    plan mode: {tasksPlanState}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  {tasksPlanState === "disabled" && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "draft", tasksEditing);
                      }}
                    >
                      Enter Plan
                    </Button>
                  )}
                  {tasksPlanState === "draft" && (
                    <Button
                      type="button"
                      size="sm"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "approved", tasksEditing);
                      }}
                    >
                      Approve
                    </Button>
                  )}
                  {tasksPlanState === "approved" && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "draft", tasksEditing);
                      }}
                    >
                      Reopen Plan
                    </Button>
                  )}
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    className="h-7 px-2 text-xs"
                    disabled={tasksLoading || tasksSaving}
                    onClick={() => {
                      setTasksEditing((value) => !value);
                      setTasksDraft(tasksContent);
                    }}
                  >
                    {tasksEditing ? "Preview" : "Edit"}
                  </Button>
                  {tasksEditing && (
                    <Button
                      type="button"
                      size="sm"
                      className="h-7 px-2 text-xs"
                      disabled={tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, tasksPlanState, false);
                      }}
                    >
                      {tasksSaving ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : null}
                      Save
                    </Button>
                  )}
                </div>
              </div>
              {tasksError && (
                <div className="mb-2 rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-2 text-xs text-destructive">
                  {tasksError}
                </div>
              )}
              {tasksLoading ? (
                <div className="text-xs text-muted-foreground">Loading tasks.md…</div>
              ) : tasksEditing ? (
                <textarea
                  value={tasksDraft}
                  onChange={(event) => setTasksDraft(event.target.value)}
                  className="min-h-[220px] w-full rounded-lg border border-border bg-background px-3 py-2 font-mono text-xs text-foreground outline-none focus:ring-2 focus:ring-ring"
                />
              ) : tasksContent.trim() ? (
                <div className="rounded-lg border border-border/60 bg-background/70 px-3 py-2">
                  <MemoMarkdown content={tasksContent} components={components} />
                </div>
              ) : (
                <div className="text-xs text-muted-foreground">tasks.md is empty.</div>
              )}
            </div>
          )}

          {!isUser && previewTargets.length > 0 && (
            <div className="not-prose mt-3 space-y-2">
              {previewTargets.map((target, index) => (
                <div
                  key={`${target.path}:${target.viewer ?? 'default'}:${index}`}
                  className="group flex items-center justify-between rounded-xl border border-border/70 bg-card px-3 py-2"
                >
                  <button
                    type="button"
                    onClick={() => onOpenPreview?.(target)}
                    className="min-w-0 flex-1 text-left"
                  >
                    <div className="truncate text-sm font-medium text-foreground">{target.path}</div>
                    <div className="text-xs text-muted-foreground">
                      {target.viewer === 'slidev' ? 'Slidev preview' : 'Preview'}
                    </div>
                  </button>
                  <a
                    href={target.path.startsWith('http://') || target.path.startsWith('https://') ? target.path : '#'}
                    target="_blank"
                    rel="noreferrer"
                    onClick={(e) => {
                      if (!(target.path.startsWith('http://') || target.path.startsWith('https://'))) {
                        e.preventDefault();
                        onOpenPreview?.(target);
                      }
                    }}
                    className="ml-3 inline-flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
                    title="Open"
                  >
                    <Download className="h-4 w-4" />
                  </a>
                </div>
              ))}
            </div>
          )}
          {!isUser && showTyping && !message.content && !message.error && (
            <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.2s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.1s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce" />
              <span className="ml-1">typing</span>
            </div>
          )}

        </div>

        <div className={cn('flex items-center gap-2 mt-1 px-1', isUser ? 'flex-row-reverse' : 'flex-row')}>
          <span className="text-[10px] text-muted-foreground">{new Date(message.timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
          {!isStreaming && (
            <CopyButton
              text={message.content}
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
            />
          )}
          {isUser && canRetry && onRetry && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
              onClick={() => onRetry()}
            >
              <RotateCcw className="h-3 w-3" />
            </Button>
          )}
        </div>

        {message.attachments?.length ? (
          <div className={cn('flex flex-wrap gap-2 mt-2', isUser ? 'justify-end' : 'justify-start')}>
            {message.attachments.map(att => (
              <AttachmentPreview key={att.id} attachment={att}>
                <AttachmentCard attachment={att} variant="message" />
              </AttachmentPreview>
            ))}
          </div>
        ) : null}
      </div>
    </motion.div>
  );
}
