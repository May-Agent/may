import { Check, Loader2, X } from 'lucide-react';
import { compactionRefusal } from '@/lib/compaction';
import { useTranslation } from '@/lib/i18n';
import type { TurnToolState } from '@/lib/stream/reducer';

/**
 * A compaction is a one-line status row with nothing to expand: a check
 * when it started, a red cross with the refusal reason when it was refused.
 */
export function CompactionRow({ tool }: { tool: Pick<TurnToolState, 'status' | 'result'> }) {
  const { t } = useTranslation();
  const streaming = tool.status === 'streaming';
  const refusal = streaming ? null : compactionRefusal(tool.result);
  return (
    <div className="flex items-center gap-2 rounded-md px-1.5 py-1 text-sm" data-compaction>
      {streaming ? (
        <Loader2 className="size-3.5 shrink-0 animate-spin text-muted-foreground" />
      ) : refusal === null ? (
        <Check className="size-3.5 shrink-0 text-muted-foreground" />
      ) : (
        <X className="size-3.5 shrink-0 text-destructive" />
      )}
      <span className="text-foreground/85">{t('tools.compact')}</span>
      {refusal !== null && <span className="text-destructive">{refusal}</span>}
    </div>
  );
}
