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

/**
 * A single timeline row (terminal command, thinking block, web search, etc).
 *
 * Visual design goals (Cursor-inspired clean aesthetic):
 *   - Collapsed: borderless, backgroundless, inline text + chevron. Reads as
 *     part of the message flow like a thinking footnote, not a card.
 *   - Expanded: subtle rounded panel with content. The expand animation
 *     morphs the container rather than pops a new card.
 *   - Running: a dim left accent bar (2px) is the *only* chrome that
 *     persists in collapsed state. No full border.
 */
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

  return (
    <div
      className={cn(
        'group/row relative',
        // Only while expanded do we dress it up as a panel.
        isExpanded
          ? 'mt-1 rounded-xl border border-border/50 bg-muted/15 px-3 py-2'
          : 'px-1 py-0.5 rounded-sm transition-colors hover:bg-muted/20',
      )}
    >
      {item.isRunning && !isExpanded && (
        <span
          aria-hidden
          className="absolute left-0 top-1/2 -translate-y-1/2 h-[60%] w-[2px] rounded-full bg-foreground/40"
        />
      )}
      <button
        type="button"
        className={cn(
          'flex w-full items-center gap-2 text-left rounded-sm',
          'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
          item.isRunning && !isExpanded ? 'pl-2' : 'pl-0',
        )}
        onClick={onToggle}
        aria-expanded={isExpanded}
        aria-controls={contentId}
      >
        <motion.span
          animate={{ rotate: isExpanded ? 90 : 0 }}
          transition={{ duration: 0.12 }}
          className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-muted-foreground/70"
          aria-hidden
        >
          <ChevronRight className="h-3.5 w-3.5" />
        </motion.span>
        <span
          className={cn(
            'min-w-0 flex-1 truncate text-[13.5px] tracking-[-0.003em]',
            item.isRunning
              ? 'text-foreground/90 font-normal'
              : 'text-muted-foreground',
          )}
        >
          {item.isRunning ? (
            <ScanningLabel>{rowSummaryText}</ScanningLabel>
          ) : (
            rowSummaryText
          )}
        </span>
        {item.isRunning ? (
          <span className="text-[11px] text-muted-foreground/80 tabular-nums">
            {durationText}
          </span>
        ) : statusLabel ? (
          <span className={cn('text-[11px] tabular-nums', timelineItemStatusClass(item))}>
            {statusLabel}
          </span>
        ) : (
          <span className="text-[11px] text-muted-foreground/60 tabular-nums">
            {durationText}
          </span>
        )}
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
