import type { ApiPr, ApiPrCheck, ApiPrChecks } from '../api/prs';

/** PR states that still have checks in flight on GitHub. */
const LIVE_STATES = ['open', 'checks_pending', 'merging'];

/** The PRs the sidebar lists: those still in flight. */
export function livePrs(prs: ApiPr[]): ApiPr[] {
  return prs.filter((pr) => LIVE_STATES.includes(pr.state));
}

/** How the rollup reads at a glance, GitHub's traffic light. */
export type PrRollup = 'unknown' | 'failed' | 'running' | 'passed' | 'merging';

export function prRollup(pr: ApiPr): PrRollup {
  if (pr.state === 'merging') return 'merging';
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
