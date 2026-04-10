import type { Dispatch, SetStateAction, RefObject } from 'react';
import { useCallback } from 'react';
import { removePendingSessionMessage } from '@/lib/api';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { Attachment, PendingUserMessage } from '@/types';

export interface UseQueuedMessageActionsInput {
  currentSessionId: string | null;
  /**
   * Full queued list as the user sees it — `pendingImmediate` merged
   * with `queuedMessages`. Needed to decide whether index 0 points at
   * pending_immediate or queued[0].
   */
  queuedMessages: PendingUserMessage[];
  setInputValue: Dispatch<SetStateAction<string>>;
  setAttachments: Dispatch<SetStateAction<Attachment[]>>;
  setError: Dispatch<SetStateAction<string | null>>;
  /** Composer textarea — focused after steering so the user can edit immediately. */
  composerInputRef: RefObject<HTMLTextAreaElement | null>;
}

export interface UseQueuedMessageActionsResult {
  removeQueuedMessage: (index: number) => Promise<void>;
  steerQueuedMessage: (index: number) => void;
}

/**
 * Encapsulates the two queue-manipulation actions users can take
 * on pending messages from the ActivityRail:
 *
 * * `removeQueuedMessage(index)` — asks the backend to drop the
 *   message at that position and applies the returned runtime.
 * * `steerQueuedMessage(index)` — hoists the queued message content
 *   and attachments back into the composer so the user can edit
 *   before resending, then removes it from the queue.
 *
 * Both handlers need to know whether the first slot is currently
 * `pending_immediate` (which lives outside `queuedMessages` on the
 * backend) so they can translate UI indices into the right kind.
 */
export function useQueuedMessageActions({
  currentSessionId,
  queuedMessages,
  setInputValue,
  setAttachments,
  setError,
  composerInputRef,
}: UseQueuedMessageActionsInput): UseQueuedMessageActionsResult {
  const applyRuntime = useRuntimeStore((state) => state.applyRuntime);
  const getRuntime = useRuntimeStore((state) => state.sessions);

  const removeQueuedMessage = useCallback(
    async (index: number): Promise<void> => {
      if (!currentSessionId) return;
      try {
        const runtime = getRuntime[currentSessionId];
        const hasPendingImmediate = runtime?.pendingImmediate != null;
        const kind =
          hasPendingImmediate && index === 0 ? 'pending_immediate' : 'queued';
        const queuedIndex = hasPendingImmediate ? index - 1 : index;
        const nextRuntime = await removePendingSessionMessage({
          sessionId: currentSessionId,
          kind,
          index: kind === 'queued' ? queuedIndex : undefined,
        });
        applyRuntime(currentSessionId, nextRuntime);
      } catch (error) {
        setError(
          `Failed to remove queued message: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      }
    },
    [applyRuntime, currentSessionId, getRuntime, setError],
  );

  const steerQueuedMessage = useCallback(
    (index: number): void => {
      const queued = queuedMessages[index];
      if (!queued) return;

      setInputValue((prev) =>
        prev.trim().length > 0
          ? `${queued.content}\n\n${prev}`
          : queued.content,
      );

      if (queued.attachments.length > 0) {
        const timestamp = Date.now();
        const restored: Attachment[] = queued.attachments.map(
          (attachment, attachmentIndex) => ({
            ...attachment,
            // Rehydrated attachments get fresh ids so they don't
            // collide with other attachments already in the composer.
            id: `${attachment.id}-queued-${timestamp}-${index}-${attachmentIndex}`,
          }),
        );
        setAttachments((prev) => [...restored, ...prev]);
      }

      void removeQueuedMessage(index);
      window.setTimeout(() => {
        composerInputRef.current?.focus();
      }, 0);
    },
    [
      composerInputRef,
      queuedMessages,
      removeQueuedMessage,
      setAttachments,
      setInputValue,
    ],
  );

  return { removeQueuedMessage, steerQueuedMessage };
}
