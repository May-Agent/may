import { Suspense, lazy, memo, useState, useMemo, useEffect, useRef } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { motion, AnimatePresence } from 'framer-motion';
import { cn } from '@/lib/utils';
import { buildSessionOutputPreviewUrl, fetchSessionTasks, saveSessionTasks } from '@/lib/api';
import type { ArtifactPreviewTarget, Message, SessionPlanState } from '@/types';
import { useStore } from '@/lib/store';
import { extractArtifactPreviewTargets, stripArtifactPreviewTags } from './chatUtils';
import ReactMarkdown from 'react-markdown';
import { Copy, Check, FileText, File, ChevronRight, RotateCcw, Download, Terminal, Globe, Search, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';
import { FlowMarkdown } from './StreamingText';

const TASKS_TAG_PATTERN = /<tasks\s*\/>/i;
const LazySyntaxCodeBlock = lazy(() => import('./SyntaxCodeBlock'));

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

function detectCodeLanguage(text: string): 'bash' | 'json' | 'text' {
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
  if (/(\$\s+|bash|zsh|sh\s)/i.test(trimmed)) return 'bash';
  return 'text';
}

function formatCodeText(text: string, language: 'bash' | 'json' | 'text'): string {
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

interface ParsedCommandImage {
  src: string;
  mediaType: string;
  source: 'inline' | 'stored';
  storagePath?: string;
}

interface ParsedCommandOutput {
  text?: string;
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

function parseOutputEntry(
  entry: unknown,
  parsed: ParsedCommandOutput,
  textChunks: string[],
  sourcePath?: string,
) {
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
      ? (content as Record<string, unknown>)
      : node;
  const kind = typeof payload.type === 'string' ? payload.type.trim().toLowerCase() : '';
  if (kind === 'text') {
    const text = typeof payload.text === 'string' ? payload.text : '';
    if (text.trim().length > 0) {
      if (isLikelyBinaryTextPayload(text)) {
        return;
      }
      textChunks.push(text);
    }
    return;
  }
  if (kind === 'image') {
    const encoded = typeof payload.data === 'string' ? payload.data.trim() : '';
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
  const parsed: ParsedCommandOutput = {
    images: [],
    storagePaths: [],
  };
  if (!output || output.trim().length === 0) {
    return parsed;
  }

  const interpreted = parseJsonLike(output);
  const textChunks: string[] = [];

  parseOutputEntry(interpreted, parsed, textChunks);

  if (textChunks.length === 0 && parsed.images.length === 0 && parsed.storagePaths.length === 0) {
    if (isLikelyBinaryTextPayload(output)) {
      return parsed;
    }
    parsed.text = formatCodeText(output, detectCodeLanguage(output));
  } else if (textChunks.length > 0) {
    const merged = textChunks.join('\n\n').trim();
    if (merged.length > 0) {
      parsed.text = formatCodeText(merged, detectCodeLanguage(merged));
    }
  }

  parsed.storagePaths = [...new Set(parsed.storagePaths)];
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
  const bashExecutions = message.bashExecutions || [];
  const [isTimelineOpen, setIsTimelineOpen] = useState(false);
  const [expandedTimelineItemIds, setExpandedTimelineItemIds] = useState<Record<string, boolean>>({});
  const hasSeenRunningTimelineRef = useRef(false);
  const [showTyping, setShowTyping] = useState(false);
  const [tasksContent, setTasksContent] = useState("");
  const [tasksDraft, setTasksDraft] = useState("");
  const [tasksLoading, setTasksLoading] = useState(false);
  const [tasksSaving, setTasksSaving] = useState(false);
  const [tasksEditing, setTasksEditing] = useState(false);
  const [tasksError, setTasksError] = useState<string | null>(null);
  const [tasksPlanState, setTasksPlanState] = useState<SessionPlanState>('disabled');
  const setSessionPlanState = useStore((state) => state.setSessionPlanState);
  const refreshProfileSessions = useStore((state) => state.refreshProfileSessions);
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
      for (const command of bashExecutions) {
        const parsedOutput = parseCommandOutput(command.output);
        const outputLang = detectCodeLanguage(parsedOutput.text || '');
        items.push({
          id: command.id,
          type: 'command',
          summary: toSingleLineSummary(command.input, 96) || '(empty command)',
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
      if (segment.kind === 'bash') {
        for (const command of segment.commands) {
          const parsedOutput = parseCommandOutput(command.output);
          const outputLang = detectCodeLanguage(parsedOutput.text || '');
          items.push({
            id: command.id,
            type: 'command',
            summary: toSingleLineSummary(command.input, 96) || '(empty command)',
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
    message.bashExecutions,
    message.id,
    message.isThinking,
    message.thinking,
    message.timelineSegments,
    message.timestamp,
    bashExecutions,
  ]);
  const hasImageLikeBashOutput = useMemo(
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
    if (hasImageLikeBashOutput) {
      return true;
    }
    return normalized.length >= 256;
  }, [displayContent, hasImageLikeBashOutput, isUser, timelineItems.length]);
  const safeDisplayContent = suppressBinaryContent ? '' : displayContent;

  useEffect(() => {
    setIsTimelineOpen(false);
    setExpandedTimelineItemIds({});
    hasSeenRunningTimelineRef.current = false;
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
      void refreshProfileSessions();
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

  const runningTimelineItemIds = timelineItems
    .filter((item) => item.isRunning)
    .map((item) => item.id);
  const runningTimelineKey = runningTimelineItemIds.join('|');
  const hasRunningTimelineItem = runningTimelineItemIds.length > 0;

  useEffect(() => {
    const runningIds = runningTimelineKey.length > 0
      ? runningTimelineKey.split('|').filter((value) => value.length > 0)
      : [];
    if (runningIds.length > 0) {
      hasSeenRunningTimelineRef.current = true;
      setIsTimelineOpen(true);
      setExpandedTimelineItemIds(
        runningIds.reduce<Record<string, boolean>>((acc, id) => {
          acc[id] = true;
          return acc;
        }, {}),
      );
      return;
    }
    if (hasSeenRunningTimelineRef.current) {
      setIsTimelineOpen(false);
      setExpandedTimelineItemIds({});
      hasSeenRunningTimelineRef.current = false;
    }
  }, [runningTimelineKey]);

  const commandCount = timelineItems.filter((item) => item.type === 'command').length;
  const thinkingCount = timelineItems.filter((item) => item.type === 'thinking').length;
  const webSearchCount = timelineItems.filter((item) => item.type === 'websearch').length;
  const webFetchCount = timelineItems.filter((item) => item.type === 'webfetch').length;
  const failedTimelineCount = timelineItems.filter((item) => item.isFailed).length;
  const runningTimelineCount = runningTimelineItemIds.length;
  const nowMs = Date.now();
  const timelineElapsedMs = timelineItems.reduce((sum, item) => {
    const startedAt = item.startedAt;
    if (!startedAt) {
      return sum;
    }
    const endedAt = item.endedAt ?? (item.isRunning ? nowMs : startedAt);
    const delta = Math.max(0, endedAt - startedAt);
    return sum + delta;
  }, 0);
  const timelineElapsedText = formatDurationMs(timelineElapsedMs);
  const summaryParts: string[] = [];
  if (commandCount > 0) {
    summaryParts.push(`Ran ${commandCount} command${commandCount > 1 ? 's' : ''}`);
  }
  if (webSearchCount > 0) {
    summaryParts.push('searched the web');
  }
  if (webFetchCount > 0) {
    summaryParts.push(`fetched ${webFetchCount} page${webFetchCount > 1 ? 's' : ''}`);
  }
  if (thinkingCount > 0 && commandCount === 0 && webSearchCount === 0 && webFetchCount === 0) {
    summaryParts.push(`Thought ${thinkingCount} time${thinkingCount > 1 ? 's' : ''}`);
  }
  const timelineSummaryText =
    summaryParts.length > 0
      ? summaryParts.join(', ')
      : `${timelineItems.length} step${timelineItems.length > 1 ? 's' : ''}`;
  const effectiveTimelineOpen = hasRunningTimelineItem || isTimelineOpen;

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
            <div className="mb-2 not-prose">
              <button
                type="button"
                className="inline-flex w-full items-center gap-2 rounded-md px-1 py-1 text-left text-zinc-400 transition-colors hover:text-zinc-200"
                onClick={() => {
                  if (hasRunningTimelineItem) {
                    return;
                  }
                  setIsTimelineOpen((prev) => !prev);
                }}
                aria-expanded={effectiveTimelineOpen}
              >
                <span className="truncate text-[15px] font-medium text-zinc-300">{timelineSummaryText}</span>
                <span className="ml-auto inline-flex items-center gap-2 text-[12px] text-zinc-500">
                  {runningTimelineCount > 0 ? (
                    <span className="text-cyan-300">{runningTimelineCount} running</span>
                  ) : failedTimelineCount > 0 ? (
                    <span className="text-red-300">{failedTimelineCount} failed</span>
                  ) : (
                    <span>done</span>
                  )}
                  <span>{timelineElapsedText}</span>
                </span>
                <motion.span
                  animate={{ rotate: effectiveTimelineOpen ? 90 : 0 }}
                  transition={{ duration: 0.15, ease: 'easeOut' }}
                  className="inline-flex h-4 w-4 items-center justify-center text-zinc-500"
                >
                  <ChevronRight className="h-3.5 w-3.5" />
                </motion.span>
              </button>

              <AnimatePresence initial={false}>
                {effectiveTimelineOpen && (
                  <motion.div
                    initial={{ height: 0, opacity: 0 }}
                    animate={{ height: 'auto', opacity: 1 }}
                    exit={{ height: 0, opacity: 0 }}
                    transition={{ duration: 0.16, ease: [0.16, 1, 0.3, 1] }}
                    className="overflow-hidden"
                  >
                    <div className="mt-1 rounded-xl border border-zinc-700/60 bg-zinc-900/45 px-3 py-2">
                      <div className="max-h-[420px] overflow-y-auto pr-1 scrollbar-none">
                        <div className="space-y-2">
                          {timelineItems.map((item, index) => {
                            const isExpanded = item.isRunning || expandedTimelineItemIds[item.id] === true;
                            const startedAt = item.startedAt ?? Date.parse(message.timestamp);
                            const normalizedStartedAt = Number.isFinite(startedAt) ? startedAt : nowMs;
                            const endedAt = item.endedAt ?? (item.isRunning ? nowMs : normalizedStartedAt);
                            const durationText = formatDurationMs(Math.max(0, endedAt - normalizedStartedAt));
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
                              <div key={item.id} className="rounded-lg border border-zinc-800 bg-zinc-900/70">
                                <button
                                  type="button"
                                  className="flex w-full items-center gap-2 px-3 py-2 text-left"
                                  onClick={() =>
                                    setExpandedTimelineItemIds((previous) => ({
                                      ...previous,
                                      [item.id]: !isExpanded,
                                    }))
                                  }
                                >
                                  <span className="text-[11px] tabular-nums text-zinc-500">{index + 1}</span>
                                  {item.type === 'command' && <Terminal className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                                  {item.type === 'thinking' && <FileText className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                                  {item.type === 'websearch' && <Search className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                                  {item.type === 'webfetch' && <Globe className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
                                  <span className="min-w-0 flex-1 truncate text-[14px] text-zinc-200">{item.summary}</span>
                                  {item.isRunning ? (
                                    <span className="inline-flex items-center gap-1 text-[11px] text-cyan-300">
                                      <Loader2 className="h-3 w-3 animate-spin" />
                                      running
                                    </span>
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
                                            <pre className="max-h-[240px] overflow-y-auto whitespace-pre-wrap break-all rounded-md border border-zinc-800 bg-zinc-950/80 p-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
                                              {item.commandInput}
                                            </pre>
                                            {item.outputText && (
                                              <pre className="max-h-[240px] overflow-y-auto whitespace-pre-wrap break-all rounded-md border border-zinc-800 bg-zinc-950/80 p-2 font-mono text-[11px] leading-relaxed text-zinc-200 scrollbar-none">
                                                {item.outputText}
                                              </pre>
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
                      </div>
                    </div>
                  </motion.div>
                )}
              </AnimatePresence>
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
              <FlowMarkdown content={safeDisplayContent} isStreaming={isStreaming} />
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
                {att.type === 'image' ? (
                  <div className="border rounded-md overflow-hidden bg-background">
                    <img src={att.url} alt={att.name} className="h-32 w-32 object-cover transition-transform hover:scale-105" />
                  </div>
                ) : (att.type === 'document' || att.type === 'pdf') ? (
                  <div className="h-20 w-48 border rounded-lg overflow-hidden bg-muted/40 p-2.5 flex flex-col gap-1">
                    <div className="flex items-center gap-1.5">
                      <FileText className={cn("h-3.5 w-3.5 flex-shrink-0", att.type === 'pdf' ? 'text-red-400' : 'text-blue-400')} />
                      <span className="text-[11px] font-medium truncate">{att.name}</span>
                    </div>
                    {att.textPreview ? (
                      <p className="text-[10px] text-muted-foreground/70 leading-tight line-clamp-3">{att.textPreview}</p>
                    ) : (
                      <p className="text-[10px] text-muted-foreground/50 uppercase tracking-wide mt-auto">{att.type === 'pdf' ? 'PDF' : 'DOCUMENT'}</p>
                    )}
                  </div>
                ) : (
                  <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted flex flex-col items-center justify-center p-2 gap-1">
                    <File className="h-6 w-6 text-muted-foreground" />
                    <span className="text-[9px] truncate w-full text-center" title={att.name}>{att.name}</span>
                  </div>
                )}
              </AttachmentPreview>
            ))}
          </div>
        ) : null}
      </div>
    </motion.div>
  );
}
