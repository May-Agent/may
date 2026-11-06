import type { ProviderModelsGroup } from './api/models';
import type { ReasoningEffortPatch } from './api/sessions';

/** The rung that clears the session's effort override instead of setting it. */
export const NO_EFFORT = 'none';

/** The rung a session sits on before it has selected one. */
export const DEFAULT_EFFORT = 'medium';

/** The effort ladder of `modelId` from the model list; empty when the model has no effort control. */
export function ladderForModel(
  groups: ProviderModelsGroup[] | null,
  modelId: string,
): string[] {
  for (const group of groups ?? []) {
    const detail = group.details.find((item) => item.id === modelId);
    if (detail) return detail.effortLadder ?? [];
  }
  return [];
}

/** The ladder index of the session's stored effort; `null` is the `none` rung. */
export function stopOfEffort(
  ladder: readonly string[],
  current: string | null | undefined,
): number {
  const level = current?.toLowerCase() ?? NO_EFFORT;
  const exact = ladder.indexOf(level);
  if (exact >= 0) return exact;
  return Math.max(ladder.indexOf(DEFAULT_EFFORT), 0);
}

/** The session patch for a rung: `none` clears the override. */
export function patchOfEffort(level: string): ReasoningEffortPatch {
  return level === NO_EFFORT ? null : level;
}

/** The stop nearest to `fraction` (0 at the track's left end, 1 at its right) of `count` evenly spaced stops. */
export function nearestStop(fraction: number, count: number): number {
  if (count <= 1) return 0;
  const clamped = Math.min(Math.max(fraction, 0), 1);
  return Math.round(clamped * (count - 1));
}

/** The position of `stop` along the track, as a fraction. */
export function stopFraction(stop: number, count: number): number {
  return count <= 1 ? 0 : stop / (count - 1);
}

/** The stop a key press moves to, or `null` for a key the slider ignores. */
export function stopAfterKey(key: string, stop: number, count: number): number | null {
  switch (key) {
    case 'ArrowLeft':
    case 'ArrowDown':
      return Math.max(stop - 1, 0);
    case 'ArrowRight':
    case 'ArrowUp':
      return Math.min(stop + 1, count - 1);
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}
