import { memo, useMemo } from 'react';
import {
  Check,
  ChevronRight,
  Globe,
  HelpCircle,
  Loader2,
  MessageSquare,
  MousePointer,
  Search,
  Sparkles,
  Terminal as TerminalIcon,
  Wrench,
  X,
} from 'lucide-react';
import { cn } from '@/lib/utils';
import type { TurnToolState } from '@/lib/stream/reducer';
import type { ToolVariant } from '@/lib/types';
import { parsePartialJson, projectField } from '@/lib/stream/partialJson';

interface ToolCardProps {
  tool: TurnToolState;
}

function variantIcon(variant: ToolVariant) {
  switch (variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_kill':
    case 'terminal_read':
      return TerminalIcon;
    case 'browser':
      return MousePointer;
    case 'web_search':
      return Search;
    case 'web_fetch':
      return Globe;
    case 'ask':
      return HelpCircle;
    case 'task':
      return Sparkles;
    case 'notify':
      return MessageSquare;
    default:
      return Wrench;
  }
}

function statusIcon(status: TurnToolState['status'], result: unknown) {
  if (status === 'streaming') {
    return <Loader2 className="size-3.5 animate-spin text-muted-foreground" />;
  }
  if (typeof result === 'object' && result !== null && 'error' in (result as object)) {
    return <X className="size-3.5 text-destructive" />;
  }
  return <Check className="size-3.5 text-emerald-500" />;
}

function TerminalBody({ tool }: { tool: TurnToolState }) {
  const command = projectField(tool.argumentsRaw, 'command');
  const description = projectField(tool.argumentsRaw, 'description');
  const mode = projectField(tool.argumentsRaw, 'mode');

  return (
    <div className="mt-1.5 space-y-1.5">
      {description.length > 0 && (
        <div className="text-xs text-muted-foreground leading-snug">{description}</div>
      )}
      {command.length > 0 && (
        <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono leading-relaxed whitespace-pre-wrap break-all">
          <span className="text-muted-foreground mr-2">$</span>
          {command}
          {tool.status === 'streaming' && (
            <span className="inline-block w-1.5 h-3 bg-foreground/70 animate-pulse ml-0.5 align-middle" />
          )}
        </pre>
      )}
      {mode && mode !== 'default' && (
        <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
          mode: {mode}
        </div>
      )}
      <TerminalOutput result={tool.result} />
    </div>
  );
}

function TerminalOutput({ result }: { result: unknown }) {
  if (!result || typeof result !== 'object') return null;
  const rec = result as Record<string, unknown>;
  const stdout = typeof rec.stdout === 'string' ? rec.stdout : '';
  const stderr = typeof rec.stderr === 'string' ? rec.stderr : '';
  const exit = typeof rec.exitCode === 'number' ? rec.exitCode : null;
  if (!stdout && !stderr && exit === null) return null;

  return (
    <details className="group">
      <summary className="flex cursor-pointer items-center gap-1 text-[11px] text-muted-foreground hover:text-foreground transition-colors list-none">
        <ChevronRight className="size-3 transition-transform group-open:rotate-90" />
        <span>
          output {exit !== null && <span className="font-mono">({exit})</span>}
        </span>
      </summary>
      <div className="mt-1.5 rounded-md border border-border bg-muted/20 p-2">
        {stdout && (
          <pre className="text-xs font-mono text-foreground/90 whitespace-pre-wrap max-h-64 overflow-auto scrollbar-thin">
            {stdout}
          </pre>
        )}
        {stderr && (
          <pre className="text-xs font-mono text-destructive/90 whitespace-pre-wrap max-h-64 overflow-auto scrollbar-thin mt-1">
            {stderr}
          </pre>
        )}
      </div>
    </details>
  );
}

function WebSearchBody({ tool }: { tool: TurnToolState }) {
  const query = projectField(tool.argumentsRaw, 'query');
  return (
    <div className="mt-1.5">
      {query && (
        <div className="text-xs">
          <span className="text-muted-foreground">searching </span>
          <span className="font-medium">“{query}”</span>
          {tool.status === 'streaming' && (
            <span className="inline-block w-1.5 h-3 bg-foreground/70 animate-pulse ml-1 align-middle" />
          )}
        </div>
      )}
    </div>
  );
}

function WebFetchBody({ tool }: { tool: TurnToolState }) {
  const url = projectField(tool.argumentsRaw, 'url');
  return (
    <div className="mt-1.5">
      {url && (
        <div className="text-xs font-mono text-muted-foreground break-all">
          {url}
          {tool.status === 'streaming' && (
            <span className="inline-block w-1.5 h-3 bg-foreground/70 animate-pulse ml-1 align-middle" />
          )}
        </div>
      )}
    </div>
  );
}

function BrowserBody({ tool }: { tool: TurnToolState }) {
  const action = projectField(tool.argumentsRaw, 'action');
  const target = projectField(tool.argumentsRaw, 'target') || projectField(tool.argumentsRaw, 'url');
  return (
    <div className="mt-1.5 text-xs text-muted-foreground">
      {action && <span className="font-medium text-foreground mr-1.5">{action}</span>}
      {target && <span className="font-mono break-all">{target}</span>}
    </div>
  );
}

function GenericBody({ tool }: { tool: TurnToolState }) {
  const parsed = useMemo(() => parsePartialJson(tool.argumentsRaw), [tool.argumentsRaw]);
  if (parsed === null || (typeof parsed === 'object' && Object.keys(parsed as object).length === 0)) {
    return null;
  }
  return (
    <pre className="mt-1.5 rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono leading-relaxed whitespace-pre-wrap break-all max-h-48 overflow-auto scrollbar-thin">
      {JSON.stringify(parsed, null, 2)}
    </pre>
  );
}

function selectBody(tool: TurnToolState) {
  switch (tool.variant) {
    case 'terminal':
    case 'terminal_input':
    case 'terminal_read':
    case 'terminal_kill':
      return <TerminalBody tool={tool} />;
    case 'web_search':
      return <WebSearchBody tool={tool} />;
    case 'web_fetch':
      return <WebFetchBody tool={tool} />;
    case 'browser':
      return <BrowserBody tool={tool} />;
    default:
      return <GenericBody tool={tool} />;
  }
}

export const ToolCard = memo(function ToolCard({ tool }: ToolCardProps) {
  const Icon = variantIcon(tool.variant);
  const body = selectBody(tool);

  return (
    <div
      className={cn(
        'rounded-lg border border-border bg-card/40 px-3 py-2 transition-colors',
        tool.status === 'streaming' && 'shadow-sm shadow-primary/5 border-primary/20',
      )}
    >
      <div className="flex items-center gap-2">
        <div className="flex size-5 items-center justify-center rounded bg-muted/80">
          <Icon className="size-3.5 text-muted-foreground" />
        </div>
        <span className="text-sm font-medium leading-none">{tool.name}</span>
        <span className="text-[11px] uppercase tracking-wider text-muted-foreground">
          {tool.variant}
        </span>
        <div className="ml-auto">{statusIcon(tool.status, tool.result)}</div>
      </div>
      {body}
    </div>
  );
});
