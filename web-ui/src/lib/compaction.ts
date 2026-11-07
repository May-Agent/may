/** Prefix of the `compact` tool's result when it started a compaction. */
const COMPACTION_STARTED = 'Compaction started';

function resultText(result: unknown): string {
  if (typeof result === 'string') return result;
  if (!result || typeof result !== 'object') return '';
  const r = result as Record<string, unknown>;
  if (r.kind === 'text' && typeof r.text === 'string') return r.text;
  if (r.kind === 'error' && typeof r.message === 'string') return r.message;
  return '';
}

/**
 * The reason a finished `compact` call was refused, or `null` when it
 * started a compaction. Any result other than the started message is a
 * refusal; its text is the reason.
 */
export function compactionRefusal(result: unknown): string | null {
  const text = resultText(result).trim();
  return text.startsWith(COMPACTION_STARTED) ? null : text;
}
