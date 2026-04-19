import type { RefObject, UIEventHandler } from 'react';
import type {
  ArtifactPreviewTarget,
  Attachment,
  BackgroundReason,
  Message,
  SessionPlanState,
} from '@/types';
import { MessageItem } from './MessageItem';
import { collectRetryBatch } from './chatUtils';

export interface RetryRequest {
  sessionIdHint: string | undefined;
  includeComposerInput: boolean;
  retryAssistantId: string | undefined;
  overrideBatch: Array<{ content: string; attachments: Attachment[] }>;
  skipAppendUserMessages: boolean;
}

interface ChatMessageListProps {
  messages: Message[];
  currentSessionId: string | null;
  scrollContainerRef: RefObject<HTMLDivElement | null>;
  contentRef: RefObject<HTMLDivElement | null>;
  onScroll: UIEventHandler<HTMLDivElement>;
  onOpenPreview: (target: ArtifactPreviewTarget) => void;
  onRetry: (request: RetryRequest) => void;
  /** Backend task_id of the terminal task currently running in background. */
  activeBackgroundTaskId?: string | null;
  /** Task IDs whose runtime state has cleared but whose tool_end hasn't landed yet. */
  recentlyBackgroundedTaskIds?: ReadonlySet<string> | null;
  /** Why the currently-active background task was promoted (if known). */
  activeBackgroundReason?: BackgroundReason | null;
  sessionPlanState?: SessionPlanState;
  onApprovePlan?: () => void;
  onReopenPlan?: () => void;
  isPlanStateUpdating?: boolean;
  onFocusComposer?: () => void;
}

export function ChatMessageList({
  messages,
  currentSessionId,
  scrollContainerRef,
  contentRef,
  onScroll,
  onOpenPreview,
  onRetry,
  activeBackgroundTaskId,
  recentlyBackgroundedTaskIds,
  activeBackgroundReason,
  sessionPlanState,
  onApprovePlan,
  onReopenPlan,
  isPlanStateUpdating,
  onFocusComposer,
}: ChatMessageListProps) {
  return (
    <div
      ref={scrollContainerRef}
      onScroll={onScroll}
      className="flex-1 min-h-0 overflow-y-auto"
    >
      <div
        ref={contentRef}
        className="max-w-3xl mx-auto px-4 space-y-6 pb-6 pt-4"
      >
        {messages.map((msg, index) => {
          const isLast = index === messages.length - 1;
          const retryFromUser = msg.role === 'user' && isLast;
          const retryFromAssistant = msg.role === 'assistant' && !!msg.error;

          return (
            <MessageItem
              key={msg.id}
              message={msg}
              sessionId={currentSessionId ?? undefined}
              canRetry={retryFromUser || retryFromAssistant}
              onOpenPreview={onOpenPreview}
              activeBackgroundTaskId={activeBackgroundTaskId ?? null}
              recentlyBackgroundedTaskIds={recentlyBackgroundedTaskIds ?? null}
              activeBackgroundReason={activeBackgroundReason ?? null}
              sessionPlanState={sessionPlanState}
              onApprovePlan={onApprovePlan}
              onReopenPlan={onReopenPlan}
              isPlanStateUpdating={isPlanStateUpdating}
              onFocusComposer={onFocusComposer}
              onRetry={() => {
                const batch = retryFromUser
                  ? [
                      {
                        content: msg.content,
                        attachments: msg.attachments ?? [],
                      },
                    ]
                  : collectRetryBatch(messages, msg.id);

                onRetry({
                  sessionIdHint: currentSessionId ?? undefined,
                  includeComposerInput: false,
                  retryAssistantId: retryFromAssistant ? msg.id : undefined,
                  overrideBatch: batch,
                  skipAppendUserMessages: true,
                });
              }}
            />
          );
        })}
      </div>
    </div>
  );
}
