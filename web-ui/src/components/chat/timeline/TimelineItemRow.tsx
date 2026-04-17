import { useId, type ReactNode } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  type TimelineDisplayItem,
  buildRunningTimelineLabel,
  buildCollapsedTimelineLabel,
  timelineItemStatusLabel,
  timelineItemStatusClass,
  formatDurationMs,
} from '@/lib/timeline';
import { ScanningLabel } from './ScanningLabel';

interface TimelineItemRowProps {
  item: TimelineDisplayItem;
  isExpanded: boolean;
  onToggle: () => void;
  nowMs: number;
  fallbackStartedAt: number;
  children: ReactNode;
  index: number;
}

export function TimelineItemRow({
  item,
  isExpanded,
  onToggle,
  nowMs,
  fallbackStartedAt,
  children,
}: TimelineItemRowProps) {
  const contentId = useId();
  const startedAt = item.startedAt ?? fallbackStartedAt;
  const normalizedStartedAt = Number.isFinite(startedAt) ? startedAt : nowMs;
  const endedAt = item.endedAt ?? (item.isRunning ? nowMs : normalizedStartedAt);
  const durationText = formatDurationMs(Math.max(0, endedAt - normalizedStartedAt));
  const rowSummaryText = item.isRunning
    ? buildRunningTimelineLabel(item)
    : buildCollapsedTimelineLabel(item);
  const statusLabel = timelineItemStatusLabel(item);

  const containerClass = isExpanded
    ? 'rounded-2xl border border-border/60 bg-muted/20 px-3 py-2'
    : 'rounded-md px-1 py-0.5 hover:bg-muted/30';

  return (
    <div
      className={cn(
        'group/row overflow-hidden transition-colors',
        containerClass,
        item.isRunning && 'border-l-2 border-l-foreground/50',
      )}
    >
      <button
        type="button"
        className="flex w-full items-center gap-2 px-0 py-0.5 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring rounded-sm"
        onClick={onToggle}
        aria-expanded={isExpanded}
        aria-controls={contentId}
      >
        <span
          className={cn(
            'min-w-0 flex-1 truncate text-[14px] tracking-[-0.005em]',
            item.isRunning ? 'font-medium' : 'text-foreground',
          )}
        >
          {item.isRunning ? (
            <ScanningLabel>{rowSummaryText}</ScanningLabel>
          ) : (
            rowSummaryText
          )}
        </span>
        {item.isRunning ? (
          <span className="text-[11px] text-muted-foreground tabular-nums">{durationText}</span>
        ) : statusLabel ? (
          <span className={cn('text-[11px]', timelineItemStatusClass(item))}>
            {statusLabel}
          </span>
        ) : (
          <span className="text-[11px] text-muted-foreground tabular-nums">{durationText}</span>
        )}
        <motion.span
          animate={{ rotate: isExpanded ? 90 : 0 }}
          transition={{ duration: 0.12 }}
          className="inline-flex h-4 w-4 items-center justify-center text-muted-foreground"
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </motion.span>
      </button>
      <AnimatePresence initial={false}>
        {isExpanded && (
          <motion.div
            id={contentId}
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: 'auto', opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.14 }}
            className="overflow-hidden"
          >
            <div className="space-y-2 pt-2 text-xs text-foreground">
              {children}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
