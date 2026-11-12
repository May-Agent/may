/** Parses the job pool field: empty is automatic, otherwise a positive whole number; undefined is invalid. */
export function parseJobPoolSize(text: string): number | null | undefined {
  const trimmed = text.trim();
  if (trimmed === '') return null;
  if (!/^\d+$/.test(trimmed)) return undefined;
  const size = Number(trimmed);
  return size >= 1 ? size : undefined;
}
