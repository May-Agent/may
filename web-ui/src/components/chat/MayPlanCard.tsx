import { ClipboardList, Loader2, Play } from 'lucide-react';
import { Streamdown } from 'streamdown';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import type { MayPlanStatus } from '@/lib/parsers/mayPlanBlocks';
import { CopyButton } from './shared/CopyButton';
import { makeMarkdownComponents } from './shared/MarkdownRenderer';
import type { SessionPlanState } from '@/types';

interface MayPlanCardProps {
  content: string;
  status: MayPlanStatus;
  open: boolean;
  sessionPlanState: SessionPlanState;
  onApprove?: () => void;
  onReopen?: () => void;
  isUpdatingPlanState?: boolean;
  onFocusComposer?: () => void;
}

interface StatusChipStyle {
  label: string;
  className: string;
}

function statusChip(
  status: MayPlanStatus,
  sessionPlanState: SessionPlanState,
): StatusChipStyle {
  if (sessionPlanState === 'approved') {
    return {
      label: status === 'blocked' ? 'blocked' : 'executing',
      className:
        'border-blue-500/30 bg-blue-500/10 text-blue-200/90 dark:text-blue-300/90',
    };
  }
  if (status === 'blocked') {
    return {
      label: 'blocked',
      className:
        'border-rose-500/30 bg-rose-500/10 text-rose-200/90 dark:text-rose-300/90',
    };
  }
  if (status === 'progress') {
    return {
      label: 'progress',
      className:
        'border-amber-500/30 bg-amber-500/10 text-amber-200/90 dark:text-amber-300/90',
    };
  }
  return {
    label: 'draft',
    className:
      'border-amber-500/30 bg-amber-500/10 text-amber-200/90 dark:text-amber-300/90',
  };
}

export function MayPlanCard({
  content,
  status,
  open,
  sessionPlanState,
  onApprove,
  onReopen,
  isUpdatingPlanState,
  onFocusComposer,
}: MayPlanCardProps) {
  const chip = statusChip(status, sessionPlanState);
  const components = makeMarkdownComponents(false, open);
  const showApprove =
    !open && sessionPlanState !== 'approved' && onApprove != null;
  const showReopen = !open && sessionPlanState === 'approved' && onReopen != null;
  const trimmedContent = content.trim();

  return (
    <div className="not-prose my-3 rounded-xl border border-border/70 bg-muted/30 p-4 shadow-[inset_0_1px_0_rgba(255,255,255,0.04)]">
      <div className="mb-3 flex items-center justify-between gap-3">
        <div className="flex items-center gap-2 text-sm font-medium text-foreground">
          <ClipboardList className="h-4 w-4 text-muted-foreground" />
          <span>Plan</span>
        </div>
        <span
          className={cn(
            'inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] font-medium tracking-wide',
            chip.className,
          )}
        >
          {open ? (
            <>
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
              drafting
            </>
          ) : (
            chip.label
          )}
        </span>
      </div>

      <div className="rounded-lg bg-background/40 px-3 py-2">
        {trimmedContent ? (
          <Streamdown
            mode={open ? 'streaming' : 'static'}
            parseIncompleteMarkdown={open}
            isAnimating={open}
            linkSafety={{ enabled: false }}
            controls={{ code: false, table: false }}
            components={components}
          >
            {content}
          </Streamdown>
        ) : (
          <div className="text-xs text-muted-foreground">
            Drafting plan…
          </div>
        )}
      </div>

      {!open && (
        <div className="mt-3 flex flex-wrap items-center justify-end gap-2">
          {onFocusComposer && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-8 px-3 text-xs"
              onClick={onFocusComposer}
            >
              Ask follow-up
            </Button>
          )}
          <CopyButton
            text={trimmedContent}
            className="h-8 w-8 text-muted-foreground hover:text-foreground"
          />
          {showReopen && (
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-8 px-3 text-xs"
              disabled={isUpdatingPlanState}
              onClick={onReopen}
            >
              {isUpdatingPlanState ? (
                <Loader2 className="mr-1 h-3 w-3 animate-spin" />
              ) : null}
              Reopen planning
            </Button>
          )}
          {showApprove && (
            <Button
              type="button"
              size="sm"
              className="h-8 px-3 text-xs"
              disabled={isUpdatingPlanState}
              onClick={onApprove}
            >
              {isUpdatingPlanState ? (
                <Loader2 className="mr-1 h-3 w-3 animate-spin" />
              ) : (
                <Play className="mr-1 h-3 w-3 fill-current" />
              )}
              Approve & execute
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
