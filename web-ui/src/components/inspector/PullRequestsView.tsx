import { useEffect, useState } from 'react';
import { Check, ChevronRight, Circle, ExternalLink, X } from 'lucide-react';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import type { ApiPr, ApiPrCheck } from '@/lib/api/prs';
import { Button } from '@/components/ui/button';
import { formatElapsed } from '@/lib/inspector/model';
import {
  checkDurationMs,
  checkProgress,
  hasPrDetail,
  isPrTerminal,
  prFailureDetail,
  prOpenedProvenance,
  prRollup,
  type PrRollup,
} from '@/lib/inspector/prChecks';
import { cn } from '@/lib/utils';

const PR_STATE_KEY: Partial<Record<string, MessageKey>> = {
  open: 'inspector.prStates.open',
  checks_pending: 'inspector.prStates.checks_pending',
  merging: 'inspector.prStates.merging',
  merged: 'inspector.prStates.merged',
  closed: 'inspector.prStates.closed',
  failed: 'inspector.prStates.failed',
  rejected: 'inspector.prStates.rejected',
};

const ROLLUP_TONE: Record<PrRollup, string> = {
  unknown: 'bg-emerald-500',
  merging: 'bg-emerald-500',
  running: 'bg-amber-500',
  failed: 'bg-destructive',
  passed: 'bg-emerald-500',
};

/** The current time, ticking each second while `active` so running checks count up. */
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    // The clock is an external source: every reading lands from a timer
    // callback, the first at once so a clock started late is not stale.
    const tick = () => setNow(Date.now());
    const first = setTimeout(tick, 0);
    const id = setInterval(tick, 1000);
    return () => {
      clearTimeout(first);
      clearInterval(id);
    };
  }, [active]);
  return now;
}

export interface PullRequestsViewProps {
  prs: ApiPr[];
  onMerge?: (prId: number) => Promise<void> | void;
  onClose?: (prId: number) => Promise<void> | void;
}

/**
 * The repository's PRs. A row shows state, orphaned/review badges, rollup and
 * progress, failure/rejection detail, and expands into provenance and checks.
 */
export function PullRequestsView({ prs, onMerge, onClose }: PullRequestsViewProps) {
  const running = prs.some((pr) => pr.checks?.items.some((c) => c.state === 'running'));
  const now = useNow(running);
  return (
    <ul className="-mx-2 space-y-1">
      {prs.map((pr) => (
        <PullRequestRow
          key={pr.id > 0 ? pr.id : `${pr.repoKey}-${pr.number}`}
          pr={pr}
          now={now}
          onMerge={onMerge}
          onClose={onClose}
        />
      ))}
    </ul>
  );
}

function PullRequestRow({
  pr,
  now,
  onMerge,
  onClose,
}: {
  pr: ApiPr;
  now: number;
  onMerge?: (prId: number) => Promise<void> | void;
  onClose?: (prId: number) => Promise<void> | void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [busyAction, setBusyAction] = useState<'merge' | 'close' | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const foldable = hasPrDetail(pr);
  const rollup = prRollup(pr);
  const progress = pr.checks ? checkProgress(pr.checks) : null;
  const failureDetail = prFailureDetail(pr);
  const opened = prOpenedProvenance(pr);
  const terminal = isPrTerminal(pr.state);
  const canUserAct = (pr.orphaned || pr.requireReview) && !terminal && pr.id > 0;

  const runAction = async (kind: 'merge' | 'close') => {
    const fn = kind === 'merge' ? onMerge : onClose;
    if (!fn) return;
    setBusyAction(kind);
    setActionError(null);
    try {
      await fn(pr.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const head = (
    <>
      {foldable ? (
        <ChevronRight
          className={cn('size-3.5 shrink-0 transition-transform', open && 'rotate-90')}
          aria-hidden
        />
      ) : (
        <span className="size-3.5 shrink-0" aria-hidden />
      )}
      <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
        {pr.backend === 'gitlab' ? '!' : '#'}
        {pr.number}
      </span>
      <span className="min-w-0 flex-1 truncate text-[13px]" title={pr.title}>
        {pr.title}
      </span>
      {pr.orphaned && (
        <span className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-600 dark:text-amber-400">
          {t('inspector.prOrphaned')}
        </span>
      )}
      {!pr.orphaned && pr.requireReview && !terminal && (
        <span className="shrink-0 rounded bg-sky-500/15 px-1.5 py-0.5 text-[10px] font-medium text-sky-600 dark:text-sky-400">
          {t('inspector.prReviewRequired')}
        </span>
      )}
      {pr.state !== 'open' && (
        <span className="shrink-0 text-[11px] text-muted-foreground">
          {(() => {
            const key = PR_STATE_KEY[pr.state];
            return key ? t(key) : pr.state;
          })()}
        </span>
      )}
      {progress && progress.total > 0 && (
        <span
          className="shrink-0 text-xs tabular-nums text-muted-foreground"
          title={t('inspector.prProgress', progress)}
        >
          {progress.finished}/{progress.total}
        </span>
      )}
      <span
        className={cn('size-2 shrink-0 rounded-full', ROLLUP_TONE[rollup])}
        role="img"
        aria-label={t(`inspector.prRollup.${rollup}`)}
        title={t(`inspector.prRollup.${rollup}`)}
      />
    </>
  );

  return (
    <li>
      {foldable ? (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="flex min-h-8 w-full items-center gap-2 rounded-md px-2 py-1 text-left hover:bg-accent/60 touch:min-h-11"
        >
          {head}
        </button>
      ) : (
        <div className="flex min-h-8 items-center gap-2 rounded-md px-2 py-1 touch:min-h-11">
          {head}
        </div>
      )}

      {failureDetail && (
        <p className="px-7 py-0.5 text-xs text-destructive break-words">{failureDetail}</p>
      )}

      {canUserAct && (onMerge || onClose) && (
        <div className="flex flex-wrap items-center gap-1.5 px-7 py-1">
          {onMerge && (
            <Button
              size="sm"
              variant="outline"
              className="h-6 px-2 text-xs"
              disabled={busyAction !== null}
              onClick={() => void runAction('merge')}
            >
              {busyAction === 'merge' ? t('inspector.prMerging') : t('inspector.prMerge')}
            </Button>
          )}
          {onClose && (
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-2 text-xs text-destructive hover:text-destructive"
              disabled={busyAction !== null}
              onClick={() => void runAction('close')}
            >
              {busyAction === 'close' ? t('inspector.prClosing') : t('inspector.prClose')}
            </Button>
          )}
          {actionError && <span className="text-xs text-destructive">{actionError}</span>}
        </div>
      )}

      {foldable && open && (
        <div className="pb-1 pl-7 pr-2 space-y-1">
          {(pr.repoKey || opened || pr.mergeCommitSha) && (
            <div className="space-y-0.5 text-[11px] text-muted-foreground">
              {pr.repoKey && <p className="truncate">{pr.repoKey}</p>}
              {opened && (
                <p className="break-words">
                  {t('inspector.prOpenedBy', {
                    actor: opened.actor || 'main',
                    session: opened.sessionId || pr.sessionId,
                    message: opened.messageId ? ` (msg-#${opened.messageId})` : '',
                    time: opened.createdAt,
                  })}
                </p>
              )}
              {pr.mergeCommitSha && <p className="font-mono">merge {pr.mergeCommitSha}</p>}
            </div>
          )}
          {pr.checksError && (
            <p className="py-1 text-xs text-destructive">
              {t('inspector.prChecksUnreadable', { message: pr.checksError })}
            </p>
          )}
          <ul>
            {pr.checks?.items.map((check) => (
              <CheckRow key={check.name} check={check} now={now} />
            ))}
          </ul>
        </div>
      )}
    </li>
  );
}

function CheckRow({ check, now }: { check: ApiPrCheck; now: number }) {
  const { t } = useTranslation();
  const duration = checkDurationMs(check, now);
  return (
    <li className="flex min-h-7 items-center gap-2 text-xs touch:min-h-10">
      <CheckMark state={check.state} label={t(`inspector.prCheck.${check.state}`)} />
      <span className="min-w-0 flex-1 truncate" title={check.name}>
        {check.name}
      </span>
      {check.state === 'failed' && check.url && (
        <a
          href={check.url}
          target="_blank"
          rel="noreferrer"
          className="shrink-0 text-muted-foreground hover:text-foreground"
          aria-label={t('inspector.prCheckDetail', { name: check.name })}
          title={t('inspector.prCheckDetail', { name: check.name })}
        >
          <ExternalLink className="size-3.5" aria-hidden />
        </a>
      )}
      <span className="w-12 shrink-0 text-right tabular-nums text-muted-foreground">
        {duration === null ? '' : formatElapsed(duration)}
      </span>
    </li>
  );
}

function CheckMark({ state, label }: { state: ApiPrCheck['state']; label: string }) {
  const common = { 'aria-label': label, role: 'img' } as const;
  switch (state) {
    case 'passed':
      return <Check className="size-3.5 shrink-0 text-emerald-500" {...common} />;
    case 'failed':
      return <X className="size-3.5 shrink-0 text-destructive" {...common} />;
    case 'running':
      return <Circle className="size-3.5 shrink-0 fill-amber-500 text-amber-500" {...common} />;
    case 'queued':
      return <Circle className="size-3.5 shrink-0 text-muted-foreground" {...common} />;
  }
}
