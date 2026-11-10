import type { ToolVariant } from './types';

/** String fields each native terminal tool requires in its arguments. */
const REQUIRED_FIELDS: Partial<Record<ToolVariant, readonly string[]>> = {
  terminal: ['script'],
  terminal_input: ['task_id', 'input'],
  terminal_kill: ['task_id'],
  terminal_read: ['task_id'],
};

/**
 * Why a completed terminal-family call's arguments do not match the tool's
 * schema, or `null` when they do (or the tool is not one of them). The
 * arguments are written by the model, so a mismatch is ordinary input: the
 * daemon returns an argument error to the model and the call shows as it was
 * written. Only call this once the arguments are complete.
 */
export function invalidToolArguments(variant: ToolVariant, argumentsRaw: string): string | null {
  const required = REQUIRED_FIELDS[variant];
  if (required === undefined) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(argumentsRaw);
  } catch (error) {
    return error instanceof Error ? error.message : 'arguments are not valid JSON';
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    return 'arguments are not a JSON object';
  }
  const record = parsed as Record<string, unknown>;
  for (const field of required) {
    if (typeof record[field] !== 'string') return `missing string field \`${field}\``;
  }
  return null;
}
