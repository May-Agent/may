// Pure helpers for timeline segment manipulation during live streaming.
// These operate on TimelineSegment arrays without touching React state,
// so they belong outside the chat components.

import type { TerminalExecution, TimelineSegment } from '@/types';

/**
 * Produce a deep-enough clone of the timeline so downstream consumers
 * can mutate nested arrays without leaking back into the streaming
 * state the hook maintains.
 */
export function cloneTimelineSegments(
  segments: TimelineSegment[],
): TimelineSegment[] {
  return segments.map((segment) =>
    segment.kind === 'terminal'
      ? {
          ...segment,
          commands: segment.commands.map((command) => ({ ...command })),
        }
      : segment.kind === 'tool'
        ? { ...segment, tool: { ...segment.tool } }
        : { ...segment },
  );
}

/**
 * Flatten terminal timeline segments into a fresh TerminalExecution
 * array, cloned so the caller can mutate without aliasing.
 */
export function collectTerminalExecutions(
  segments: TimelineSegment[],
): TerminalExecution[] {
  return segments.flatMap((segment) =>
    segment.kind === 'terminal'
      ? segment.commands.map((command) => ({ ...command }))
      : [],
  );
}

/**
 * Concatenate the content of all thinking segments, trimmed.
 * Used to populate the legacy `thinking` field on completed messages.
 */
export function collectThinkingText(segments: TimelineSegment[]): string {
  return segments
    .filter(
      (segment): segment is Extract<TimelineSegment, { kind: 'thinking' }> =>
        segment.kind === 'thinking',
    )
    .map((segment) => segment.content)
    .join('\n\n')
    .trim();
}

/**
 * True when any tool or terminal segment has a `stopped` status —
 * i.e. the user interrupted mid-run.
 */
export function timelineContainsStoppedWork(
  segments: TimelineSegment[],
): boolean {
  return segments.some((segment) =>
    segment.kind === 'tool'
      ? segment.tool.status === 'stopped'
      : segment.kind === 'terminal'
        ? segment.commands.some((command) => command.status === 'stopped')
        : false,
  );
}

/** True when any thinking segment is still marked as running. */
export function isAnyThinkingRunning(segments: TimelineSegment[]): boolean {
  return segments.some(
    (segment) => segment.kind === 'thinking' && segment.isRunning,
  );
}

/**
 * Mark the thinking segment with the given id as ended.
 * Returns a fresh array; segments with other ids are left untouched.
 */
export function closeThinkingSegment(
  segments: TimelineSegment[],
  segmentId: string,
  endedAt: number,
): TimelineSegment[] {
  return segments.map((segment) =>
    segment.kind === 'thinking' && segment.id === segmentId
      ? {
          ...segment,
          isRunning: false,
          endedAt: segment.endedAt ?? endedAt,
        }
      : segment,
  );
}
