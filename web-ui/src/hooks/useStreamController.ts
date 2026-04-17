import type { Dispatch, SetStateAction } from 'react';
import { useCallback, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import {
  enqueueChat,
  fetchSessionRuntime,
  isBrowserTakeoverActiveError,
  isSessionActiveConflictError,
  stopSessionRun,
  streamChat,
} from '@/lib/api';
import { useStore } from '@/lib/store';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { Attachment, Message } from '@/types';
import {
  collectRetryBatch,
  settleTimelineSegments,
  summarizeTimelineSegments,
  type QueuedMessage,
} from '@/components/chat/chatUtils';
import {
  cloneTimelineSegments,
  collectTerminalExecutions,
  collectThinkingText,
  isAnyThinkingRunning,
  timelineContainsStoppedWork,
} from '@/lib/streamTimeline';
import {
  isRuntimeTargetBusy,
  validateModelConfigured,
} from '@/lib/streamValidation';
import {
  createStreamEventHandlers,
  createStreamState,
  type AssistantFinalizeUpdate,
} from '@/lib/streamEventHandlers';

const TEXT_FLUSH_MIN_INTERVAL_MS = 50;

export interface SendOptions {
  sessionIdHint?: string;
  includeComposerInput?: boolean;
  retryAssistantId?: string;
  overrideBatch?: QueuedMessage[];
  skipAppendUserMessages?: boolean;
  deliveryMode?: 'steer' | 'queue';
}

export interface UseStreamControllerInput {
  // Session identity and configuration
  currentSessionId: string | null;
  newChatModelId: string;
  configuredModelIds: ReadonlySet<string>;

  // Composer state (read and written)
  inputValue: string;
  attachments: Attachment[];
  setInputValue: Dispatch<SetStateAction<string>>;
  setAttachments: Dispatch<SetStateAction<Attachment[]>>;

  // UI state the controller drives
  setUiError: Dispatch<SetStateAction<string | null>>;
  setIsSending: Dispatch<SetStateAction<boolean>>;
  setIsStreamControllerActive: Dispatch<SetStateAction<boolean>>;
  setIsBrowserOpen: Dispatch<SetStateAction<boolean>>;
  setTodoProgress: Dispatch<
    SetStateAction<{
      items: Array<{ text: string; checked: boolean }>;
      total: number;
      completed: number;
    } | null>
  >;

  // Scroll ref the controller resets when sending
  userScrolledUpRef: React.MutableRefObject<boolean>;

  // Session store actions
  createSession: (model?: string) => Promise<string>;
  addMessage: (
    sessionId: string,
    content: string,
    role: 'user' | 'assistant',
    attachments?: Attachment[],
  ) => void;
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void;
}

export interface UseStreamControllerResult {
  handleSend: (options?: SendOptions) => Promise<void>;
  handleStop: () => void;
}

/**
 * Centralized controller for chat stream lifecycle: sending,
 * streaming, stopping, and rolling back optimistic updates.
 *
 * Owns all the mutable refs that the old inline handleSend/handleStop
 * shared (AbortController, streaming session id, stop-request set,
 * stop-waiter promises). Callers wire up composer state and session
 * store actions via the input object and receive a stable pair of
 * handleSend/handleStop callbacks.
 */
export function useStreamController(
  input: UseStreamControllerInput,
): UseStreamControllerResult {
  const navigate = useNavigate();

  const abortControllerRef = useRef<AbortController | null>(null);
  const streamingSessionIdRef = useRef<string | null>(null);
  const stopRequestedSessionIdsRef = useRef<Set<string>>(new Set());
  const stopWaitersRef = useRef<Map<string, Promise<void>>>(new Map());

  const markRuntimeRunning = useRuntimeStore((state) => state.markRunning);
  const applyRuntime = useRuntimeStore((state) => state.applyRuntime);
  const markRuntimeFirstToken = useRuntimeStore(
    (state) => state.markFirstToken,
  );
  const clearRecentlyBackgroundedTaskId = useRuntimeStore(
    (state) => state.clearRecentlyBackgroundedTaskId,
  );

  const syncSessionRuntime = useCallback(async (sessionId: string) => {
    const runtime = await fetchSessionRuntime(sessionId);
    applyRuntime(sessionId, runtime);
    return runtime;
  }, [applyRuntime]);

  /** Ask the backend to stop the session and apply the returned runtime. */
  const waitForStoppedSession = useCallback(
    async (sessionId: string): Promise<void> => {
      const runtime = await stopSessionRun(sessionId);
      applyRuntime(sessionId, runtime);
    },
    [applyRuntime],
  );

  const handleSend = useCallback(
    async (options?: SendOptions): Promise<void> => {
      const includeComposerInput = options?.includeComposerInput ?? true;
      const composerHasInput =
        input.inputValue.trim().length > 0 || input.attachments.length > 0;
      if (includeComposerInput && !composerHasInput) return;

      const requestedSessionId =
        options?.sessionIdHint ?? input.currentSessionId;

      // Wait for any in-flight stop before sending a new message.
      const pendingStop = requestedSessionId
        ? stopWaitersRef.current.get(requestedSessionId)
        : undefined;
      if (pendingStop) {
        try {
          await pendingStop;
        } catch (error) {
          input.setUiError(
            `Failed to stop session: ${
              error instanceof Error ? error.message : String(error)
            }`,
          );
          return;
        }
      }

      // Pre-stream model validation.
      if (!requestedSessionId) {
        const validation = validateModelConfigured(
          input.newChatModelId,
          input.configuredModelIds,
          'No model configured. Please select a model before sending.',
          'Selected model is not available in the current profile.',
        );
        if (!validation.ok) {
          input.setUiError(validation.error!);
          return;
        }
      } else {
        const targetSession = useStore
          .getState()
          .sessions.find((session) => session.id === requestedSessionId);
        const validation = validateModelConfigured(
          targetSession?.model,
          input.configuredModelIds,
          'No model configured for this session. Please select a model.',
          'Session model is no longer available. Please choose another model.',
        );
        if (!validation.ok) {
          input.setUiError(validation.error!);
          return;
        }
      }

      input.setIsSending(true);
      input.setUiError(null);

      // Snapshot composer draft and clear it optimistically.
      const content = includeComposerInput ? input.inputValue : '';
      const atts = includeComposerInput ? [...input.attachments] : [];
      if (includeComposerInput) {
        input.setInputValue('');
        input.setAttachments([]);
      }

      const restoreDraft = () => {
        if (!includeComposerInput) return;
        input.setInputValue(content);
        input.setAttachments(atts);
      };

      // Queue path: if the target session is already running, enqueue
      // and return without starting a new stream.
      const targetRuntime = options?.sessionIdHint
        ? useRuntimeStore.getState().sessions[options.sessionIdHint]
        : input.currentSessionId
          ? useRuntimeStore.getState().sessions[input.currentSessionId]
          : undefined;
      const targetGenerating =
        !!targetRuntime && isRuntimeTargetBusy(targetRuntime);
      const deliveryMode =
        options?.deliveryMode ?? (targetGenerating ? 'queue' : 'steer');

      if (targetGenerating) {
        if (!includeComposerInput) {
          input.setIsSending(false);
          return;
        }
        const runtimeSessionId =
          options?.sessionIdHint ?? input.currentSessionId;
        if (runtimeSessionId) {
          try {
            const runtime = await enqueueChat({
              sessionId: runtimeSessionId,
              content,
              attachments: atts,
              deliveryMode,
            });
            applyRuntime(runtimeSessionId, runtime);
          } catch (error) {
            const message =
              error instanceof Error ? error.message : String(error);
            input.setUiError(message);
            restoreDraft();
          }
        }
        input.setIsSending(false);
        return;
      }

      // Resolve or create the target session.
      let sessionId = options?.sessionIdHint ?? input.currentSessionId;
      if (!sessionId) {
        try {
          sessionId = await input.createSession(input.newChatModelId || undefined);
        } catch (error) {
          const message =
            error instanceof Error ? error.message : String(error);
          input.setUiError(message);
          restoreDraft();
          input.setIsSending(false);
          return;
        }
        if (sessionId) {
          navigate(`/chat/${encodeURIComponent(sessionId)}`);
        }
      }

      if (!sessionId) {
        input.setIsSending(false);
        return;
      }

      stopRequestedSessionIdsRef.current.delete(sessionId);

      // Re-check the model after session creation, since the session may
      // have been assigned a different default than the UI selection.
      const resolvedSession = useStore
        .getState()
        .sessions.find((session) => session.id === sessionId);
      const resolvedValidation = validateModelConfigured(
        resolvedSession?.model,
        input.configuredModelIds,
        'No model configured for this session. Please select a model.',
        'Session model is no longer available. Please choose another model.',
      );
      if (!resolvedValidation.ok) {
        input.setIsSending(false);
        input.setUiError(resolvedValidation.error!);
        return;
      }

      // Assemble the batch of user messages to stream.
      const pendingBatch: QueuedMessage[] = options?.overrideBatch
        ? [...options.overrideBatch]
        : includeComposerInput
          ? [{ content, attachments: atts }]
          : [];

      if (pendingBatch.length === 0) {
        input.setIsSending(false);
        return;
      }

      const baseMessages =
        useStore.getState().sessions.find((session) => session.id === sessionId)
          ?.messages ?? [];

      if (!options?.skipAppendUserMessages) {
        for (const queued of pendingBatch) {
          input.addMessage(sessionId, queued.content, 'user', queued.attachments);
        }
      }

      input.userScrolledUpRef.current = false;
      streamingSessionIdRef.current = sessionId;
      markRuntimeRunning(sessionId);

      await runStream({
        sessionId,
        pendingBatch,
        baseMessages,
        options,
        content,
        atts,
        includeComposerInput,
      });
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      applyRuntime,
      input.addMessage,
      input.attachments,
      input.configuredModelIds,
      input.createSession,
      input.currentSessionId,
      input.inputValue,
      input.newChatModelId,
      input.setAttachments,
      input.setInputValue,
      input.setIsSending,
      input.setUiError,
      markRuntimeRunning,
      navigate,
    ],
  );

  /**
   * Runs the actual streamChat call and owns the finally-block cleanup.
   * Pulled out as a named helper so handleSend reads top-down instead
   * of nesting an 800-line try/catch/finally inline.
   */
  const runStream = useCallback(
    async (params: {
      sessionId: string;
      pendingBatch: QueuedMessage[];
      baseMessages: Message[];
      options: SendOptions | undefined;
      content: string;
      atts: Attachment[];
      includeComposerInput: boolean;
    }): Promise<void> => {
      const { sessionId, pendingBatch, baseMessages, options, includeComposerInput } = params;

      const controller = new AbortController();
      abortControllerRef.current = controller;
      input.setIsStreamControllerActive(true);
      const assistantId =
        options?.retryAssistantId ?? `assistant-${Date.now()}`;
      let textFlushTimer: number | null = null;
      let lastTextFlushAt = 0;
      input.setTodoProgress(null);

      const streamState = createStreamState();

      const wasStoppedByUser = (): boolean =>
        controller.signal.aborted ||
        stopRequestedSessionIdsRef.current.has(sessionId);

      const restoreComposerDraft = () => {
        if (
          !includeComposerInput ||
          options?.retryAssistantId ||
          pendingBatch.length !== 1
        ) {
          return;
        }
        const [draft] = pendingBatch;
        if (!draft) return;
        input.setInputValue(draft.content);
        input.setAttachments(draft.attachments);
      };

      const rollbackOptimisticStreamStart = async (
        message: string,
      ): Promise<void> => {
        input.replaceSessionMessages(sessionId, baseMessages);
        restoreComposerDraft();
        try {
          const runtime = await fetchSessionRuntime(sessionId);
          applyRuntime(sessionId, runtime);
        } catch (runtimeError) {
          input.setUiError(
            `Failed to refresh session state: ${
              runtimeError instanceof Error
                ? runtimeError.message
                : String(runtimeError)
            }`,
          );
          return;
        }
        input.setUiError(message);
      };

      try {
        const current = useStore
          .getState()
          .sessions.find((s) => s.id === sessionId);
        if (!current) return;

        const isRetry = !!options?.retryAssistantId;
        const retryBatch = isRetry
          ? collectRetryBatch(current.messages, assistantId)
          : [];
        const streamingBatch = isRetry ? retryBatch : pendingBatch;

        if (streamingBatch.length === 0) return;

        const finalPrompt = streamingBatch.map((m) => m.content).join('\n\n');
        const mergedAttachments = streamingBatch.flatMap((m) => m.attachments);

        // Seed the assistant placeholder (either a retry of an existing id
        // or a fresh append).
        seedAssistantPlaceholder(
          current.messages,
          sessionId,
          assistantId,
          isRetry,
          input.replaceSessionMessages,
        );

        const flushAssistantText = (force = false) => {
          const now = Date.now();
          const elapsed = now - lastTextFlushAt;

          if (!force && elapsed < TEXT_FLUSH_MIN_INTERVAL_MS) {
            if (textFlushTimer == null) {
              textFlushTimer = window.setTimeout(() => {
                textFlushTimer = null;
                flushAssistantText(true);
              }, TEXT_FLUSH_MIN_INTERVAL_MS - elapsed);
            }
            return;
          }

          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;

          lastTextFlushAt = now;
          const snapshotTimelineSegments = cloneTimelineSegments(
            streamState.timelineSegments,
          );
          const snapshotTerminalExecutions = collectTerminalExecutions(
            snapshotTimelineSegments,
          );
          const snapshotThinking = collectThinkingText(snapshotTimelineSegments);
          input.replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...m,
                    content: streamState.streamedText,
                    isStreaming: true,
                    isThinking: isAnyThinkingRunning(snapshotTimelineSegments),
                    thinking: snapshotThinking,
                    timelineSegments: snapshotTimelineSegments,
                    terminalExecutions: snapshotTerminalExecutions,
                  }
                : m,
            ),
          );
        };

        const applyAssistantFinalize = (
          update: AssistantFinalizeUpdate,
          errorMessage?: string,
        ) => {
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          input.replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...update.assistant,
                    id: assistantId,
                    content: update.finalContent,
                    thinking: update.finalThinking,
                    timelineSegments: update.finalTimelineSegments,
                    isStreaming: false,
                    isThinking: false,
                    terminalExecutions: update.finalTerminalExecutions,
                    error: errorMessage,
                  }
                : m,
            ),
          );
        };

        const clearTextFlushTimer = () => {
          if (textFlushTimer != null) {
            window.clearTimeout(textFlushTimer);
            textFlushTimer = null;
          }
        };

        const streamHandlers = createStreamEventHandlers({
          sessionId,
          assistantId,
          state: streamState,
          flushAssistantText,
          wasStoppedByUser,
          markRuntimeFirstToken,
          clearRecentlyBackgroundedTaskId,
          onTodoUpdate: (event) => input.setTodoProgress(event),
          onAssistantFinalized: (update) => {
            clearTextFlushTimer();
            applyAssistantFinalize(
              update,
              update.isEmpty
                ? 'LLM endpoint error: empty assistant response'
                : undefined,
            );
          },
          onAssistantPaused: (update) => {
            clearTextFlushTimer();
            if (update == null) {
              const latest = useStore
                .getState()
                .sessions.find((s) => s.id === sessionId);
              if (latest) {
                input.replaceSessionMessages(
                  sessionId,
                  latest.messages.filter((m) => m.id !== assistantId),
                );
              }
              return;
            }
            applyAssistantFinalize(update, undefined);
          },
          onStreamError: (message) => {
            applyStreamErrorToAssistant(
              sessionId,
              assistantId,
              streamState.timelineSegments,
              message,
              input.replaceSessionMessages,
            );
            input.setUiError(`LLM endpoint error: ${message}`);
          },
        });

        await streamChat({
          sessionId,
          content: finalPrompt,
          attachments: mergedAttachments,
          signal: controller.signal,
          onReasoning: streamHandlers.onReasoning,
          onToolDelta: streamHandlers.onToolDelta,
          onToolStart: streamHandlers.onToolStart,
          onToolEnd: streamHandlers.onToolEnd,
          onTodoUpdate: streamHandlers.onTodoUpdate,
          onText: streamHandlers.onText,
          onDone: streamHandlers.onDone,
          onPaused: streamHandlers.onPaused,
          onError: streamHandlers.onError,
        });
      } catch (error) {
        if (wasStoppedByUser()) {
          streamState.completedNormally = true;
          return;
        }
        if (error instanceof DOMException && error.name === 'AbortError') {
          return;
        }
        if (isSessionActiveConflictError(error)) {
          await rollbackOptimisticStreamStart(
            'This session is still active. Wait for the current run to settle or stop it before sending another message.',
          );
          return;
        }
        if (isBrowserTakeoverActiveError(error)) {
          await rollbackOptimisticStreamStart(
            'Browser takeover is active for this session. Return browser control before sending another message.',
          );
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        const errText = `LLM endpoint error: ${message}`;
        applyStreamErrorToAssistant(
          sessionId,
          assistantId,
          streamState.timelineSegments,
          message,
          input.replaceSessionMessages,
        );
        input.setUiError(errText);
      } finally {
        if (textFlushTimer != null) {
          window.clearTimeout(textFlushTimer);
          textFlushTimer = null;
        }

        finalizeStreamedAssistant({
          sessionId,
          assistantId,
          completedNormally: streamState.completedNormally,
          stopRequestedByUser: stopRequestedSessionIdsRef.current.has(sessionId),
          replaceSessionMessages: input.replaceSessionMessages,
        });

        abortControllerRef.current = null;
        input.setIsStreamControllerActive(false);
        input.setIsSending(false);
        if (streamingSessionIdRef.current === sessionId) {
          streamingSessionIdRef.current = null;
        }
        input.setIsBrowserOpen(false);

        void syncSessionRuntime(sessionId)
          .catch((error) => {
            input.setUiError(
              `Failed to sync runtime state: ${
                error instanceof Error ? error.message : String(error)
              }`,
            );
          })
          .finally(() => {
            stopRequestedSessionIdsRef.current.delete(sessionId);
          });
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      applyRuntime,
      clearRecentlyBackgroundedTaskId,
      input.replaceSessionMessages,
      input.setAttachments,
      input.setInputValue,
      input.setIsBrowserOpen,
      input.setIsSending,
      input.setIsStreamControllerActive,
      input.setTodoProgress,
      input.setUiError,
      markRuntimeFirstToken,
      syncSessionRuntime,
    ],
  );

  const handleStop = useCallback((): void => {
    const runtimeSessionId =
      streamingSessionIdRef.current ?? input.currentSessionId;
    if (!runtimeSessionId) return;

    const ownsLocalStream =
      streamingSessionIdRef.current === runtimeSessionId;
    stopRequestedSessionIdsRef.current.add(runtimeSessionId);
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    input.setIsStreamControllerActive(false);
    input.setIsSending(false);
    input.setIsBrowserOpen(false);

    const waitForStop = waitForStoppedSession(runtimeSessionId).finally(() => {
      stopWaitersRef.current.delete(runtimeSessionId);
      if (!ownsLocalStream) {
        stopRequestedSessionIdsRef.current.delete(runtimeSessionId);
      }
    });
    stopWaitersRef.current.set(runtimeSessionId, waitForStop);
    void waitForStop.catch((error) => {
      input.setUiError(
        `Failed to stop session: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    });

    // Mark the first currently-streaming assistant message as stopped
    // so the UI reflects the interrupt immediately, without waiting
    // for the runtime poll to settle.
    markStreamingAssistantStopped(
      runtimeSessionId,
      input.replaceSessionMessages,
    );

    streamingSessionIdRef.current = null;
  }, [
    input.currentSessionId,
    input.replaceSessionMessages,
    input.setIsBrowserOpen,
    input.setIsSending,
    input.setIsStreamControllerActive,
    input.setUiError,
    waitForStoppedSession,
  ]);

  return { handleSend, handleStop };
}

// ── Helpers ─────────────────────────────────────────────────

function seedAssistantPlaceholder(
  currentMessages: Message[],
  sessionId: string,
  assistantId: string,
  isRetry: boolean,
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void,
): void {
  if (isRetry) {
    replaceSessionMessages(
      sessionId,
      currentMessages.map((m) =>
        m.id === assistantId
          ? {
              ...m,
              content: '',
              error: undefined,
              isStreaming: true,
              isThinking: true,
              thinking: '',
              timelineSegments: [],
              terminalExecutions: [],
            }
          : m,
      ),
    );
    return;
  }
  replaceSessionMessages(sessionId, [
    ...currentMessages,
    {
      id: assistantId,
      role: 'assistant',
      content: '',
      timestamp: new Date().toISOString(),
      attachments: [],
      isStreaming: true,
      isThinking: true,
      thinking: '',
      timelineSegments: [],
      terminalExecutions: [],
    },
  ]);
}

function applyStreamErrorToAssistant(
  sessionId: string,
  assistantId: string,
  timelineSegments: import('@/types').TimelineSegment[],
  errorMessage: string,
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void,
): void {
  const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
  if (!latest) return;
  const snapshotTimelineSegments = cloneTimelineSegments(timelineSegments);
  replaceSessionMessages(
    sessionId,
    latest.messages.map((m) =>
      m.id === assistantId
        ? {
            ...m,
            timelineSegments: snapshotTimelineSegments,
            thinking: collectThinkingText(snapshotTimelineSegments),
            terminalExecutions: collectTerminalExecutions(
              snapshotTimelineSegments,
            ),
            isStreaming: false,
            isThinking: false,
            error: `LLM endpoint error: ${errorMessage}`,
          }
        : m,
    ),
  );
}

function finalizeStreamedAssistant(params: {
  sessionId: string;
  assistantId: string;
  completedNormally: boolean;
  stopRequestedByUser: boolean;
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void;
}): void {
  const { sessionId, assistantId, completedNormally, stopRequestedByUser, replaceSessionMessages } =
    params;
  const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
  if (!latest) return;
  const assistantStopped = latest.messages.some(
    (message) =>
      message.id === assistantId &&
      timelineContainsStoppedWork(message.timelineSegments ?? []),
  );
  const terminalError =
    !completedNormally && !stopRequestedByUser && !assistantStopped
      ? 'LLM endpoint error: stream terminated before a final assistant response'
      : undefined;
  replaceSessionMessages(
    sessionId,
    latest.messages.map((m) =>
      m.id === assistantId
        ? {
            ...m,
            isStreaming: false,
            isThinking: false,
            error: m.error ?? terminalError,
          }
        : m,
    ),
  );
}

function markStreamingAssistantStopped(
  sessionId: string,
  replaceSessionMessages: (sessionId: string, messages: Message[]) => void,
): void {
  const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
  if (!latest) return;
  let stopped = false;
  const endedAt = Date.now();
  replaceSessionMessages(
    sessionId,
    latest.messages.map((m) => {
      if (
        !stopped &&
        m.role === 'assistant' &&
        (m.isStreaming || m.isThinking)
      ) {
        stopped = true;
        const timelineSegments = settleTimelineSegments(
          m.timelineSegments,
          endedAt,
          'stopped',
        );
        const { thinking, terminalExecutions } =
          summarizeTimelineSegments(timelineSegments);
        return {
          ...m,
          timelineSegments,
          thinking,
          terminalExecutions,
          isStreaming: false,
          isThinking: false,
        };
      }
      return m;
    }),
  );
}
