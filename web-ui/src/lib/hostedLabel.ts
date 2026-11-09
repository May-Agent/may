import type { ToolSummary } from './types';

/** The one-row label of a search or fetch call, or `null` for any other variant. */
export function searchFetchLabel(
  variant: string,
  summary: ToolSummary | undefined,
  fallback: string,
): string | null {
  if (variant === 'web_search') return summary?.query ? `Searched "${summary.query}"` : fallback;
  if (variant === 'web_fetch') return summary?.url ? `Fetched ${summary.url}` : fallback;
  return null;
}
