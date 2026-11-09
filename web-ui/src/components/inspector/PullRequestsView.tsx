import { useEffect, useState } from 'react';
import { Check, ChevronRight, Circle, ExternalLink, X } from 'lucide-react';
import { useTranslation } from '@/lib/i18n';
import type { ApiPr, ApiPrCheck } from '@/lib/api/prs';
import { formatElapsed } from '@/lib/inspector/model';
import {
  checkDurationMs,
  checkProgress,
  hasCheckDetail,
  prRollup,
  type PrRollup,
} from '@/lib/inspector/prChecks';
import { cn } from '@/lib/utils';

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
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [active]);
  return now;
}

/**
 * The session's in-flight PRs. A row shows the rollup and the finished-of-
 * total figure; a row with checks expands into each check's state and
 * duration, a failed one linking to its detail on GitHub.
 */
export function PullRequestsView({ prs }: { prs: ApiPr[] }) {
  const running = prs.some((pr) => pr.checks?.items.some((c) => c.state === 'running'));
  const now = useNow(running);
  return (
    <ul className="-mx-2">
      {prs.map((pr) => (
        <PullRequestRow key={pr.number} pr={pr} now={now} />
      ))}
    </ul>
  );
}

function PullRequestRow({ pr, now }: { pr: ApiPr; now: number }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const foldable = hasCheckDetail(pr);
  const rollup = prRollup(pr);
  const progress = pr.checks ? checkProgress(pr.checks) : null;
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
      <span className="shrink-0 text-xs tabular-nums text-muted-foreground">#{pr.number}</span>
      <span className="min-w-0 flex-1 truncate text-[13px]" title={pr.title}>
        {pr.title}
      </span>
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
      {foldable && open && (
        <div className="pb-1 pl-7 pr-2">
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
