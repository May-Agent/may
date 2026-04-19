import { ClipboardList, Loader2, Play, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import type { SessionPlanState } from '@/types';

interface PlanModeBannerProps {
  planState: SessionPlanState;
  isUpdating?: boolean;
  onApprove: () => void;
  onReopen: () => void;
  onExit: () => void;
}

export function PlanModeBanner({
  planState,
  isUpdating,
  onApprove,
  onReopen,
  onExit,
}: PlanModeBannerProps) {
  if (planState === 'disabled') return null;

  const isDraft = planState === 'draft';
  return (
    <div
      className={cn(
        'mb-3 flex items-center justify-between gap-3 rounded-lg border px-3 py-2 text-sm',
        isDraft
          ? 'border-amber-500/30 bg-amber-500/10 text-amber-100/95'
          : 'border-blue-500/30 bg-blue-500/10 text-blue-100/95',
      )}
    >
      <div className="flex min-w-0 items-center gap-2">
        <ClipboardList className="h-4 w-4 flex-shrink-0 opacity-80" />
        <span className="truncate">
          {isDraft
            ? 'Planning mode — May will draft, not execute.'
            : 'Executing approved plan.'}
        </span>
      </div>
      <div className="flex items-center gap-1">
        {isDraft ? (
          <>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-7 px-2 text-xs text-inherit hover:bg-amber-500/20 hover:text-inherit"
              disabled={isUpdating}
              onClick={onApprove}
            >
              {isUpdating ? (
                <Loader2 className="mr-1 h-3 w-3 animate-spin" />
              ) : (
                <Play className="mr-1 h-3 w-3 fill-current" />
              )}
              Approve & execute
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="h-7 px-2 text-xs text-inherit hover:bg-amber-500/20 hover:text-inherit"
              disabled={isUpdating}
              onClick={onExit}
            >
              <X className="mr-1 h-3 w-3" />
              Exit
            </Button>
          </>
        ) : (
          <Button
            type="button"
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-xs text-inherit hover:bg-blue-500/20 hover:text-inherit"
            disabled={isUpdating}
            onClick={onReopen}
          >
            {isUpdating ? (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            ) : (
              <ClipboardList className="mr-1 h-3 w-3" />
            )}
            Reopen planning
          </Button>
        )}
      </div>
    </div>
  );
}
