import { Check, Globe, Loader2, Search, X } from 'lucide-react';
import type { TurnToolState } from '@/lib/stream/reducer';

/**
 * A search or fetch call is one row — `Searched "query"`, `Fetched url` —
 * with its status mark and nothing to unfold.
 */
export function SearchRow({
  tool,
  label,
}: {
  tool: Pick<TurnToolState, 'variant' | 'status' | 'result'>;
  label: string;
}) {
  const streaming = tool.status === 'streaming';
  const failed =
    !streaming &&
    typeof tool.result === 'object' &&
    tool.result !== null &&
    (tool.result as Record<string, unknown>).kind === 'error';
  const Icon = tool.variant === 'web_fetch' ? Globe : Search;
  return (
    <div className="flex items-center gap-2 rounded-md px-1.5 py-1 text-sm" data-search-row>
      <Icon className="size-3.5 shrink-0 text-muted-foreground" />
      <span
        className={streaming ? 'truncate shimmer-text font-medium' : 'truncate text-foreground/85'}
        title={label}
      >
        {label}
      </span>
      <span className="ml-auto shrink-0">
        {streaming ? (
          <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
        ) : failed ? (
          <X className="size-3.5 text-destructive" />
        ) : (
          <Check className="size-3.5 text-muted-foreground" />
        )}
      </span>
    </div>
  );
}
