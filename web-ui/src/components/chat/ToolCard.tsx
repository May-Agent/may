import { memo, useEffect, useMemo, useRef } from 'react';
import {
  ArrowUpRight,
  Check,
  ChevronRight,
  Globe,
  HelpCircle,
  Loader2,
  MessageSquare,
  MousePointer,
  Play,
  Search,
  Sparkles,
  Wrench,
  X,
} from 'lucide-react';
import { PrismLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import bash from 'react-syntax-highlighter/dist/esm/languages/prism/bash';
import oneLight from 'react-syntax-highlighter/dist/esm/styles/prism/one-light';
import { cn } from '@/lib/utils';
import type { TurnToolState } from '@/lib/stream/reducer';
import type { ToolVariant } from '@/lib/types';
import { parsePartialJson, projectField } from '@/lib/stream/partialJson';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import { outputsPreviewUrl } from '@/lib/api/artifacts';

SyntaxHighlighter.registerLanguage('bash', bash);

interface ToolCardProps {
  tool: TurnToolState;
}

const VARIANT_ICON: Record<ToolVariant, typeof Wrench> = {
  terminal: Play,
  terminal_input: Wrench,
  terminal_kill: Wrench,
  terminal_read: Wrench,
  browser: MousePointer,
  web_search: Search,
  web_fetch: Globe,
  ask: HelpCircle,
  session: MessageSquare,
  notify: MessageSquare,
  task: Sparkles,
  generic: Wrench,
};

const VARIANT_LABEL_KEY: Record<ToolVariant, MessageKey> = {
  terminal: 'tools.terminal',
  terminal_input: 'tools.terminalInput',
  terminal_kill: 'tools.terminalKill',
  terminal_read: 'tools.terminalRead',
  browser: 'tools.browser',
  web_search: 'tools.webSearch',
  web_fetch: 'tools.webFetch',
  ask: 'tools.ask',
  session: 'tools.session',
  notify: 'tools.notify',
  task: 'tools.task',
  generic: 'tools.generic',
};

function firstLine(text: string): string {
  const nl = text.indexOf('\n');
  return nl === -1 ? text : text.slice(0, nl);
}

function isTerminalVariant(variant: ToolVariant): boolean {
  return (
    variant === 'terminal' ||
    variant === 'terminal_input' ||
    variant === 'terminal_read' ||
    variant === 'terminal_kill'
  );
}

function pickPrimaryLabel(tool: TurnToolState, fallback: string): string {
  // The backend projects authoritative summary fields from the cumulative
  // tool arguments (`partial-json-fixer` server-side). The frontend never
  // re-parses argumentsRaw to pick a label — that path is bug-prone and
  // got the description/script ordering wrong twice. Trust tool.summary.
  const s = tool.summary ?? {};
  switch (tool.variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_read':
    case 'terminal_kill': {
      if (s.description) return s.description;
      if (s.script) return firstLine(s.script);
      return fallback;
    }
    case 'web_search':
      return s.query ? `“${s.query}”` : fallback;
    case 'web_fetch':
      return s.url ?? fallback;
    case 'browser': {
      if (s.action && s.target) return `${s.action} ${s.target}`;
      return s.action || s.target || fallback;
    }
    case 'task':
      return s.description || (s.prompt ? firstLine(s.prompt) : fallback);
    default:
      return s.description ?? fallback;
  }
}

interface OutputStream {
  text: string;
  truncated: boolean;
  /**
   * Relative outputs/* path when the full payload is persisted to disk.
   * Truncation in itself is an internal context-management concern and is
   * never surfaced to the user as a warning; we only expose it indirectly
   * by offering a link to the full file when one exists.
   */
  url: string | null;
}

interface TerminalResult {
  exitCode: number | null;
  stdout: OutputStream | null;
  stderr: OutputStream | null;
}

function extractOutputStream(stream: unknown): OutputStream | null {
  if (!stream || typeof stream !== 'object') return null;
  const e = stream as Record<string, unknown>;
  const file =
    e.file && typeof e.file === 'object' ? (e.file as Record<string, unknown>) : null;
  const url = file && typeof file.url === 'string' && file.url.length > 0 ? file.url : null;
  if (e.kind === 'inline') {
    return typeof e.text === 'string' && e.text.length > 0
      ? { text: e.text, truncated: false, url: null }
      : null;
  }
  if (e.kind === 'stored') {
    if (typeof e.preview !== 'string' || e.preview.length === 0) return null;
    return { text: e.preview, truncated: e.truncated === true, url };
  }
  return null;
}

function extractTerminalResult(result: unknown): TerminalResult | null {
  if (!result || typeof result !== 'object') return null;
  const r = result as Record<string, unknown>;
  if (r.kind !== 'command') return null;
  const outcome =
    r.outcome && typeof r.outcome === 'object' ? (r.outcome as Record<string, unknown>) : {};
  return {
    exitCode: outcome.status === 'exited' && typeof outcome.code === 'number' ? outcome.code : null,
    stdout: extractOutputStream(r.stdout),
    stderr: extractOutputStream(r.stderr),
  };
}

function extractGenericResultText(result: unknown): string | null {
  if (!result || typeof result !== 'object') return null;
  const r = result as Record<string, unknown>;
  const kind = typeof r.kind === 'string' ? r.kind : null;
  if (kind === 'text' || kind === 'tsv') {
    return typeof r.text === 'string' && r.text.length > 0 ? r.text : null;
  }
  if (kind === 'error') {
    return typeof r.message === 'string' ? r.message : null;
  }
  if (kind === 'done') return null;
  return typeof result === 'string' ? result : JSON.stringify(result, null, 2);
}

function hasError(result: unknown): boolean {
  if (!result || typeof result !== 'object') return false;
  const r = result as Record<string, unknown>;
  if (r.kind === 'error') return true;
  const terminal = extractTerminalResult(result);
  return terminal !== null && terminal.exitCode !== null && terminal.exitCode !== 0;
}

function statusIcon(status: TurnToolState['status'], result: unknown) {
  if (status === 'streaming') {
    return <Loader2 className="size-3.5 animate-spin text-muted-foreground" />;
  }
  if (hasError(result)) {
    return <X className="size-3.5 text-destructive" />;
  }
  return <Check className="size-3.5 text-emerald-500" />;
}

const shellHighlighterStyle: React.CSSProperties = {
  margin: 0,
  padding: 0,
  background: 'transparent',
  fontSize: 'inherit',
  lineHeight: 'inherit',
  flex: 1,
  minWidth: 0,
};

function ShellBlock({ script }: { script: string }) {
  return (
    <div className="flex gap-2 text-xs font-mono leading-relaxed">
      <span className="shrink-0 text-muted-foreground select-none">$</span>
      <div className="flex-1 min-w-0 max-h-64 overflow-auto scrollbar-thin">
        <SyntaxHighlighter
          language="bash"
          style={oneLight}
          customStyle={shellHighlighterStyle}
          codeTagProps={{ style: { background: 'transparent', fontFamily: 'inherit' } }}
          PreTag="div"
          wrapLongLines
          className="whitespace-pre-wrap break-all"
        >
          {script}
        </SyntaxHighlighter>
      </div>
    </div>
  );
}

function OutputBlock({
  text,
  fullOutputHref,
  tone = 'stdout',
  label,
}: {
  text: string;
  /** When provided, renders an unobtrusive link to the full stored payload. */
  fullOutputHref?: string;
  tone?: 'stdout' | 'stderr';
  label?: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="space-y-0.5">
      {label && (
        <div className="text-[10px] uppercase tracking-wider text-muted-foreground">
          {label}
        </div>
      )}
      <pre
        className={cn(
          'whitespace-pre-wrap text-xs font-mono max-h-64 overflow-auto scrollbar-thin',
          tone === 'stderr' ? 'text-destructive/90' : 'text-foreground/80',
        )}
      >
        {text}
      </pre>
      {fullOutputHref && (
        <a
          href={fullOutputHref}
          target="_blank"
          rel="noreferrer noopener"
          className="inline-flex items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground"
        >
          {t('tools.viewFullOutput')}
          <ArrowUpRight className="size-3" />
        </a>
      )}
    </div>
  );
}

function TerminalDetails({
  tool,
  primary,
}: {
  tool: TurnToolState;
  primary: string;
}) {
  const { t } = useTranslation();
  const sessionId = useSessionsStore((s) => s.active?.id ?? null);
  const script = tool.summary?.script ?? '';
  const mode = projectField(tool.argumentsRaw, 'mode');
  const escalated = mode === 'unsafe';
  const terminal = extractTerminalResult(tool.result);
  const scriptRedundant = script.length > 0 && firstLine(script) === primary;
  const showScript = script.length > 0 && !scriptRedundant;
  const hasOutput =
    terminal !== null &&
    (terminal.stdout !== null || terminal.stderr !== null || terminal.exitCode !== null);
  if (!showScript && !escalated && !hasOutput) return null;
  const failed =
    terminal !== null && terminal.exitCode !== null && terminal.exitCode !== 0;

  const hrefFor = (stream: OutputStream): string | undefined =>
    sessionId && stream.url ? outputsPreviewUrl(sessionId, stream.url) : undefined;

  return (
    <div className="space-y-2">
      {showScript && <ShellBlock script={script} />}
      {escalated && (
        <div className="text-[11px] uppercase tracking-wider text-amber-600 dark:text-amber-400">
          {t('tools.escalated')}
        </div>
      )}
      {hasOutput && terminal && (
        <div className="space-y-2">
          {terminal.stdout && (
            <OutputBlock
              text={terminal.stdout.text}
              fullOutputHref={hrefFor(terminal.stdout)}
              label={terminal.stderr !== null ? 'stdout' : undefined}
            />
          )}
          {terminal.stderr && (
            <OutputBlock
              text={terminal.stderr.text}
              fullOutputHref={hrefFor(terminal.stderr)}
              tone="stderr"
              label="stderr"
            />
          )}
          {failed && (
            <div className="text-[11px] text-destructive">
              exit {terminal.exitCode}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function GenericDetails({ tool }: { tool: TurnToolState }) {
  const parsed = useMemo(() => parsePartialJson(tool.argumentsRaw), [tool.argumentsRaw]);
  const hasArgs =
    parsed !== null &&
    typeof parsed === 'object' &&
    Object.keys(parsed as object).length > 0;
  const resultText = extractGenericResultText(tool.result);
  if (!hasArgs && resultText === null) return null;
  return (
    <div className="space-y-2">
      {hasArgs && (
        <pre className="text-xs font-mono text-foreground/80 whitespace-pre-wrap break-all max-h-48 overflow-auto scrollbar-thin">
          {JSON.stringify(parsed, null, 2)}
        </pre>
      )}
      {resultText !== null && <OutputBlock text={resultText} />}
    </div>
  );
}

function selectDetails(tool: TurnToolState, primary: string) {
  switch (tool.variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_read':
    case 'terminal_kill':
      return <TerminalDetails tool={tool} primary={primary} />;
    default:
      return <GenericDetails tool={tool} />;
  }
}

export const ToolCard = memo(function ToolCard({ tool }: ToolCardProps) {
  const { t } = useTranslation();
  const detailsRef = useRef<HTMLDetailsElement>(null);
  const Icon = VARIANT_ICON[tool.variant] ?? Wrench;
  const variantLabel = t(VARIANT_LABEL_KEY[tool.variant] ?? 'tools.generic');
  const primary = pickPrimaryLabel(tool, variantLabel);
  const streaming = tool.status === 'streaming';
  const details = selectDetails(tool, primary);
  const streamTerminalDetails = streaming && isTerminalVariant(tool.variant);

  useEffect(() => {
    if (streamTerminalDetails) {
      detailsRef.current?.setAttribute('open', '');
    }
  }, [streamTerminalDetails]);

  return (
    <details ref={detailsRef} className="group">
      <summary
        className={cn(
          'flex items-center gap-2 rounded-md px-1.5 py-1 cursor-pointer list-none',
          'text-sm transition-colors hover:bg-muted/40',
        )}
      >
        <Icon
          className={cn(
            'size-3.5 shrink-0',
            streaming ? 'text-foreground/70' : 'text-muted-foreground',
          )}
        />
        <span
          className={cn(
            'truncate',
            streaming ? 'shimmer-text font-medium' : 'text-foreground/85',
          )}
          title={primary}
        >
          {primary}
        </span>
        <span className="ml-auto flex items-center gap-1.5 shrink-0">
          {tool.variant === 'browser' && (
            <button
              type="button"
              className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-muted hover:text-foreground"
              onClick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                useBrowserViewStore.getState().setOpen(true);
              }}
            >
              <ArrowUpRight className="size-3" />
              {t('browserLive.openInToolCard')}
            </button>
          )}
          {statusIcon(tool.status, tool.result)}
          <ChevronRight className="size-3.5 text-muted-foreground transition-transform group-open:rotate-90" />
        </span>
      </summary>
      {details && <div className="mt-1.5 ml-5 space-y-2">{details}</div>}
    </details>
  );
});
