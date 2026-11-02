import { useState } from 'react';
import { ChevronDown, Sparkles } from 'lucide-react';
import { cn, formatThoughtDuration } from '@/lib/utils';
import { useTranslation } from '@/lib/i18n';

interface ReasoningBlockProps {
  text: string;
  /** Seconds the reasoning phase lasted; shown once it closed. */
  seconds?: number;
  streaming?: boolean;
}

/**
 * Reasoning/thinking header with an expandable body.
 *
 * Content streams visibly while the phase runs, then the block collapses
 * and stays manually expandable. A phase that produced no text renders as
 * a plain "thought for Ns" marker with no expand affordance.
 */
export function ReasoningBlock({ text, seconds, streaming }: ReasoningBlockProps) {
  const expandable = text.length > 0;
  const { t } = useTranslation();
  // Content is visible while the phase streams and collapses when it
  // closes; a click pins the user's choice over the streaming default.
  const [manualOpen, setManualOpen] = useState<boolean | null>(null);
  const open = manualOpen ?? Boolean(streaming);

  if (!expandable && seconds == null) return null;

  const label = streaming
    ? `${t('chat.thinking')}…`
    : seconds != null
      ? t('chat.thoughtFor', { duration: formatThoughtDuration(seconds) })
      : t('chat.thought');

  if (!expandable) {
    return (
      <div className="rounded-lg border border-dashed border-border/60 bg-muted/20 px-3 py-2">
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Sparkles className="size-3.5" />
          <span className="font-medium">{label}</span>
        </div>
      </div>
    );
  }

  return (
    <div className="rounded-lg border border-dashed border-border/60 bg-muted/20 px-3 py-2">
      <button
        type="button"
        onClick={() => setManualOpen(!open)}
        className="flex items-center gap-2 text-xs text-muted-foreground hover:text-foreground transition-colors w-full"
      >
        <Sparkles className={cn('size-3.5', streaming && 'animate-pulse')} />
        <span className={streaming ? 'shimmer-text font-medium' : 'font-medium'}>{label}</span>
        <ChevronDown className={cn('size-3 ml-auto transition-transform', open && 'rotate-180')} />
      </button>
      {open && (
        <div className="mt-1.5 text-xs leading-relaxed text-muted-foreground whitespace-pre-wrap">
          {text}
        </div>
      )}
    </div>
  );
}
