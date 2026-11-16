import type { ApiPr, ApiPrCheck, ApiPrChecks, ApiPrEvent } from '../api/prs';

/** PR states that still have checks in flight on GitHub. */
const LIVE_STATES = ['open', 'checks_pending', 'merging'];
const TERMINAL_STATES = ['merged', 'closed'];

/** The PRs the sidebar lists: those still in flight. */
export function livePrs(prs: ApiPr[]): ApiPr[] {
  return prs.filter((pr) => LIVE_STATES.includes(pr.state));
}

export function isPrTerminal(state: string): boolean {
  return TERMINAL_STATES.includes(state);
}

/** How the rollup reads at a glance, GitHub's traffic light. */
export type PrRollup = 'unknown' | 'failed' | 'running' | 'passed' | 'merging';

export function prRollup(pr: ApiPr): PrRollup {
  if (pr.state === 'merging') return 'merging';
  if (pr.state === 'failed' || pr.state === 'rejected') return 'failed';
  if (pr.state === 'merged') return 'passed';
  const checks = pr.checks;
  if (!checks) return 'unknown';
  if (checks.failed > 0) return 'failed';
  if (checks.pending > 0) return 'running';
  return 'passed';
}

/** Checks that concluded, over every check: the collapsed row's figure. */
export function checkProgress(checks: ApiPrChecks): { finished: number; total: number } {
  return {
    finished: checks.passed + checks.failed,
    total: checks.passed + checks.failed + checks.pending,
  };
}

/** Milliseconds a check has run (running) or took (concluded) as of `nowMs`. */
export function checkDurationMs(check: ApiPrCheck, nowMs: number): number | null {
  if (!check.startedAt) return null;
  const end = check.finishedAt ? Date.parse(check.finishedAt) : nowMs;
  const span = end - Date.parse(check.startedAt);
  return Number.isFinite(span) ? Math.max(0, span) : null;
}

/** Whether `prs` has anything to expand: checks, or a failed read to show. */
export function hasCheckDetail(pr: ApiPr): boolean {
  return (pr.checks?.items.length ?? 0) > 0 || pr.checksError !== null;
}

/** Whether `pr` has any details (checks, events, provenance, or SHA) to expand. */
export function hasPrDetail(pr: ApiPr): boolean {
  return (
    hasCheckDetail(pr) ||
    pr.events.length > 0 ||
    pr.mergeCommitSha !== null ||
    pr.orphaned ||
    pr.repoKey.length > 0 ||
    prFailureDetail(pr) !== null
  );
}

/** Latest rejection, check-failure, or close detail recorded on the PR. */
export function prFailureDetail(pr: ApiPr): string | null {
  if (pr.state === 'failed' || pr.state === 'rejected' || pr.state === 'closed') {
    for (let i = pr.events.length - 1; i >= 0; i--) {
      const evt = pr.events[i];
      if (
        (evt.kind === 'rejected' ||
          evt.kind === 'check_failed' ||
          evt.kind === 'removed_from_queue' ||
          (pr.state === 'closed' && evt.kind === 'closed')) &&
        evt.detail &&
        evt.detail.trim().length > 0
      ) {
        return evt.detail;
      }
    }
  }
  return null;
}

/** Opening provenance event (`opened` or earliest event). */
export function prOpenedProvenance(pr: ApiPr): ApiPrEvent | null {
  return pr.events.find((e) => e.kind === 'opened') ?? pr.events[0] ?? null;
}
