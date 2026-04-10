import { useEffect } from 'react';
import type { Message, TimelineSegment } from '@/types';
import {
  settleTimelineSegments,
  summarizeTimelineSegments,
} from '@/components/chat/chatUtils';

export interface UseSettleOrphanedTimelinesInput {
  currentSessionId: string | null;
  messages: Message[];
  /** True when a stream is currently active — we must not settle anything yet. */
  isStreamActive: boolean;
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void;
}

/**
 * Detect assistant messages that still claim to be streaming or have
 * running timeline segments while no stream is active, and settle
 * them. This handles the case where a prior stream was terminated
 * unexpectedly (network drop, browser reload mid-run, etc.) — the
 * UI shouldn't show a spinner forever.
 */
export function useSettleOrphanedTimelines({
  currentSessionId,
  messages,
  isStreamActive,
  replaceSessionMessages,
}: UseSettleOrphanedTimelinesInput): void {
  useEffect(() => {
    if (!currentSessionId || messages.length === 0) return;
    if (isStreamActive) return;

    const settledAt = Date.now();
    let changed = false;
    const nextMessages = messages.map((message) => {
      if (message.role !== 'assistant') return message;

      const timeline: TimelineSegment[] = message.timelineSegments ?? [];
      const hasRunningTimeline = timeline.some((segment) =>
        segment.kind === 'thinking'
          ? segment.isRunning
          : segment.kind === 'terminal'
            ? segment.commands.some((command) => command.status === 'running')
            : segment.tool.status === 'running',
      );
      if (!message.isStreaming && !message.isThinking && !hasRunningTimeline) {
        return message;
      }

      changed = true;
      const settledTimeline = settleTimelineSegments(timeline, settledAt);
      const { thinking, terminalExecutions } =
        summarizeTimelineSegments(settledTimeline);
      return {
        ...message,
        isStreaming: false,
        isThinking: false,
        timelineSegments: settledTimeline,
        thinking,
        terminalExecutions,
      };
    });

    if (changed) {
      replaceSessionMessages(currentSessionId, nextMessages);
    }
  }, [currentSessionId, isStreamActive, messages, replaceSessionMessages]);
}
