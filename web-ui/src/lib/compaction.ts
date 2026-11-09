/**
 * The reason a finished `compact` call was refused, or `null` when it
 * started a compaction. The tool returns a refusal as a typed error result.
 */
export function compactionRefusal(result: unknown): string | null {
  if (typeof result !== 'object' || result === null) return null;
  const r = result as Record<string, unknown>;
  return r.kind === 'error' && typeof r.message === 'string' ? r.message : null;
}
