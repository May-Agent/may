import { parse, Allow, MalformedJSON } from 'partial-json';

export function parsePartialJson(fragment: string): unknown {
  const trimmed = fragment.trim();
  if (trimmed.length === 0) return {};
  try {
    return parse(trimmed, Allow.ALL);
  } catch (err) {
    if (err instanceof MalformedJSON) return null;
    return null;
  }
}

export function projectField(fragment: string, field: string): string {
  const parsed = parsePartialJson(fragment);
  if (parsed === null || typeof parsed !== 'object') return '';
  const record = parsed as Record<string, unknown>;
  const value = record[field];
  return typeof value === 'string' ? value : '';
}
