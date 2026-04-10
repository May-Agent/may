import { CircleX } from 'lucide-react';
import { Button } from '@/components/ui/button';

interface ContainerPreparingScreenProps {
  isPrepareFailed: boolean;
  summary: string;
  recentLogs: readonly string[];
  profileSessionsLoading: boolean;
  requiresManagedPodmanApproval: boolean;
  onRetry: () => void;
  onApproveManagedPodmanDownload: () => void;
}

function DotsSpinner() {
  return (
    <div className="mx-auto mb-4 flex w-fit items-center gap-2">
      <span className="h-2.5 w-2.5 rounded-full bg-primary/70 animate-bounce [animation-delay:-240ms]" />
      <span className="h-2.5 w-2.5 rounded-full bg-primary/60 animate-bounce [animation-delay:-120ms]" />
      <span className="h-2.5 w-2.5 rounded-full bg-primary/50 animate-bounce" />
    </div>
  );
}

/**
 * Shown while a container profile is being bootstrapped (or has
 * failed). Surfaces the prepare summary and the tail of the container
 * logs so the user can see what's happening during a slow build or
 * diagnose a failure without hunting in the terminal.
 */
export function ContainerPreparingScreen({
  isPrepareFailed,
  summary,
  recentLogs,
  profileSessionsLoading,
  requiresManagedPodmanApproval,
  onRetry,
  onApproveManagedPodmanDownload,
}: ContainerPreparingScreenProps) {
  return (
    <div className="flex-1 min-h-0 px-4 pt-12">
      <div className="mx-auto w-full max-w-3xl rounded-2xl border border-border/50 bg-background/70 p-6 text-center">
        {isPrepareFailed ? (
          <div className="mx-auto mb-4 flex w-fit items-center justify-center rounded-full bg-red-500/15 p-2.5">
            <CircleX className="h-6 w-6 text-red-500" />
          </div>
        ) : (
          <DotsSpinner />
        )}
        <h2 className="text-lg font-semibold">
          {isPrepareFailed
            ? 'Container workspace unavailable'
            : 'Preparing container workspace'}
        </h2>
        <p className="mt-2 text-sm text-muted-foreground">
          {isPrepareFailed
            ? 'Unable to open chat history right now. Fix the reported error and retry.'
            : 'This profile is not ready yet. New chat and chat history are temporarily unavailable.'}
        </p>
        <div className="mt-4 rounded-lg bg-muted/50 px-3 py-2 text-left text-xs text-muted-foreground">
          {summary}
        </div>
        {recentLogs.length > 0 && (
          <div className="mt-3 max-h-40 overflow-auto rounded-lg border border-border/50 bg-background/50 px-3 py-2 text-left font-mono text-[11px] text-muted-foreground">
            {recentLogs.map((line, index) => (
              <div key={`${index}-${line}`} className="truncate">
                {line}
              </div>
            ))}
          </div>
        )}
        {isPrepareFailed && !profileSessionsLoading && (
          <div className="mt-4 flex items-center justify-center gap-2">
            {requiresManagedPodmanApproval && (
              <Button onClick={onApproveManagedPodmanDownload}>
                Allow Podman Download
              </Button>
            )}
            <Button variant="secondary" onClick={onRetry}>
              Retry
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
