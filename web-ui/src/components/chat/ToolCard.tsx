import { memo, useMemo } from 'react';
import {
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
import { cn } from '@/lib/utils';
import type { TurnToolState } from '@/lib/stream/reducer';
import type { ToolVariant } from '@/lib/types';
import { parsePartialJson, projectField } from '@/lib/stream/partialJson';
import { useTranslation, type MessageKey } from '@/lib/i18n';

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

function pickPrimaryLabel(tool: TurnToolState, fallback: string): string {
  const raw = tool.argumentsRaw;
  switch (tool.variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_read':
    case 'terminal_kill': {
      const description = projectField(raw, 'description');
      if (description.length > 0) return description;
      const script = projectField(raw, 'script');
      if (script.length > 0) return firstLine(script);
      return fallback;
    }
    case 'web_search': {
      const query = projectField(raw, 'query');
      return query.length > 0 ? `“${query}”` : fallback;
    }
    case 'web_fetch': {
      const url = projectField(raw, 'url');
      return url.length > 0 ? url : fallback;
    }
    case 'browser': {
      const action = projectField(raw, 'action');
      const target = projectField(raw, 'target') || projectField(raw, 'url');
      if (action.length > 0 && target.length > 0) return `${action} ${target}`;
      return action || target || fallback;
    }
    case 'task': {
      const description = projectField(raw, 'description');
      if (description.length > 0) return description;
      const prompt = projectField(raw, 'prompt');
      if (prompt.length > 0) return firstLine(prompt);
      return fallback;
    }
    default: {
      const description = projectField(raw, 'description');
      return description.length > 0 ? description : fallback;
    }
  }
}

interface OutputStream {
  text: string;
  truncated: boolean;
}

interface TerminalResult {
  exitCode: number | null;
  stdout: OutputStream | null;
  stderr: OutputStream | null;
}

function extractOutputStream(entry: unknown): OutputStream | null {
  if (!entry || typeof entry !== 'object') return null;
  const content = (entry as Record<string, unknown>).content;
  if (!content || typeof content !== 'object') return null;
  const c = content as Record<string, unknown>;
  if (c.type !== 'text' || typeof c.text !== 'string') return null;
  if (c.text.length === 0) return null;
  return { text: c.text, truncated: c.truncated === true };
}

function extractTerminalResult(result: unknown): TerminalResult | null {
  if (!result || typeof result !== 'object') return null;
  const r = result as Record<string, unknown>;
  if (r.kind !== 'json' || !r.value || typeof r.value !== 'object') return null;
  const v = r.value as Record<string, unknown>;
  return {
    exitCode: typeof v.exit_code === 'number' ? v.exit_code : null,
    stdout: extractOutputStream(v.stdout),
    stderr: extractOutputStream(v.stderr),
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
  if (kind === 'json') {
    if (r.value === undefined || r.value === null) return null;
    return JSON.stringify(r.value, null, 2);
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

function TerminalDetails({ tool }: { tool: TurnToolState }) {
  const { t } = useTranslation();
  const script = projectField(tool.argumentsRaw, 'script');
  const mode = projectField(tool.argumentsRaw, 'mode');
  const escalated = mode === 'unsafe';
  const terminal = extractTerminalResult(tool.result);

  return (
    <div className="space-y-1.5">
      {script.length > 0 && (
        <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono leading-relaxed whitespace-pre-wrap break-all">
          {script}
          {tool.status === 'streaming' && (
            <span className="inline-block w-1.5 h-3 bg-foreground/70 animate-pulse ml-0.5 align-middle" />
          )}
        </pre>
      )}
      {escalated && (
        <div className="text-[11px] uppercase tracking-wider text-amber-600 dark:text-amber-400">
          {t('tools.escalated')}
        </div>
      )}
      {terminal && <TerminalOutput result={terminal} />}
    </div>
  );
}

function TerminalOutput({ result }: { result: TerminalResult }) {
  const { t } = useTranslation();
  if (!result.stdout && !result.stderr && result.exitCode === null) return null;
  const failed = result.exitCode !== null && result.exitCode !== 0;

  return (
    <div className="rounded-md border border-border bg-muted/20 p-2">
      <div className="text-[11px] text-muted-foreground">
        {t('tools.output')}
        {failed && <span className="ml-1 text-destructive">· exit {result.exitCode}</span>}
      </div>
      {result.stdout && (
        <pre className="mt-1 text-xs font-mono text-foreground/90 whitespace-pre-wrap max-h-64 overflow-auto scrollbar-thin">
          {result.stdout.text}
          {result.stdout.truncated && (
            <span className="block text-[11px] text-muted-foreground">
              {t('tools.truncated')}
            </span>
          )}
        </pre>
      )}
      {result.stderr && (
        <pre className="mt-1 text-xs font-mono text-destructive/90 whitespace-pre-wrap max-h-64 overflow-auto scrollbar-thin">
          {result.stderr.text}
          {result.stderr.truncated && (
            <span className="block text-[11px] text-muted-foreground">
              {t('tools.truncated')}
            </span>
          )}
        </pre>
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
    <div className="space-y-1.5">
      {hasArgs && (
        <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono leading-relaxed whitespace-pre-wrap break-all max-h-48 overflow-auto scrollbar-thin">
          {JSON.stringify(parsed, null, 2)}
        </pre>
      )}
      {resultText !== null && <GenericOutput text={resultText} />}
    </div>
  );
}

function GenericOutput({ text }: { text: string }) {
  const { t } = useTranslation();
  return (
    <div className="rounded-md border border-border bg-muted/20 p-2">
      <div className="text-[11px] text-muted-foreground">{t('tools.output')}</div>
      <pre className="mt-1 text-xs font-mono text-foreground/90 whitespace-pre-wrap max-h-48 overflow-auto scrollbar-thin">
        {text}
      </pre>
    </div>
  );
}

function selectDetails(tool: TurnToolState) {
  switch (tool.variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_read':
    case 'terminal_kill':
      return <TerminalDetails tool={tool} />;
    default:
      return <GenericDetails tool={tool} />;
  }
}

export const ToolCard = memo(function ToolCard({ tool }: ToolCardProps) {
  const { t } = useTranslation();
  const Icon = VARIANT_ICON[tool.variant] ?? Wrench;
  const variantLabel = t(VARIANT_LABEL_KEY[tool.variant] ?? 'tools.generic');
  const primary = pickPrimaryLabel(tool, variantLabel);
  const streaming = tool.status === 'streaming';
  const details = selectDetails(tool);

  return (
    <details className="group">
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
          {statusIcon(tool.status, tool.result)}
          <ChevronRight className="size-3.5 text-muted-foreground transition-transform group-open:rotate-90" />
        </span>
      </summary>
      {details && (
        <div className="mt-1 ml-5 pl-2 border-l border-border/60 py-1 space-y-1.5">
          {details}
        </div>
      )}
    </details>
  );
});
