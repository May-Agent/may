import { useRef, useEffect, useState, useCallback, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import { AnimatePresence, motion } from "framer-motion";
import { useStore } from "@/lib/store";
import {
  fetchSessionRuntime,
  isBrowserTakeoverActiveError,
  isSessionActiveConflictError,
  enqueueChat,
  removePendingSessionMessage,
  respondAskUserRequest,
  respondPermissionRequest,
  respondWorkspaceRequest,
  setSessionPlanState as apiSetSessionPlanState,
  stopSessionRun,
  streamChat,
  type AskUserAnswer,
  type AskUserRequest,
  type PermissionRequest,
  type WorkspaceRequest,
} from "@/lib/api";
import { ArtifactsSidebar } from "./ArtifactsSidebar";
import { BrowserView } from "./BrowserView";
import { ActivityRail } from "./ActivityRail";
import {
  PermissionPrompt,
  WorkspacePrompt,
  AskUserPrompt,
  type AskUserState,
} from "./PromptBanners";
import { ChatHeader } from "./ChatHeader";
import { Composer } from "./Composer";
import { ChatMessageList } from "./ChatMessageList";
import { WelcomeScreen } from "./WelcomeScreen";
import { TodoProgressBar, type TodoProgressState } from "./TodoProgressBar";
import {
  buildAttachmentFromFile,
  buildAttachmentFromPastedFile,
  buildAttachmentFromPastedText,
} from "@/lib/attachments";
import { useArtifactPanel } from "@/hooks/useArtifactPanel";
import {
  cloneTimelineSegments,
  collectTerminalExecutions,
  collectThinkingText,
  closeThinkingSegment,
  isAnyThinkingRunning,
  timelineContainsStoppedWork,
} from "@/lib/streamTimeline";
import {
  isRuntimeTargetBusy,
  validateModelConfigured,
} from "@/lib/streamValidation";
import {
  collectRetryBatch,
  parseToolResultPayload,
  settleTimelineSegments,
  summarizeTimelineSegments,
  ARTIFACT_PANEL_MIN_WIDTH,
  ARTIFACT_PANEL_DEFAULT_WIDTH,
  ARTIFACT_PANEL_MIN_CHAT_WIDTH,
  ARTIFACT_PANEL_MAX_VIEWPORT_RATIO,
  LONG_TEXT_THRESHOLD,
  type QueuedMessage,
} from "./chatUtils";

import { Button } from "@/components/ui/button";
import { CircleX } from "lucide-react";
import type {
  ArtifactPreviewTarget,
  Attachment,
  TerminalExecution,
  ToolExecution,
  TimelineSegment,
  SessionPlanState,
} from "@/types";
import { useRuntimeStore } from "@/lib/runtimeStore";
import {
  isManagedPodmanApprovalCode,
  resolveWorkspaceSurfaceState,
} from "@/lib/containerPrepare";
import { getModel } from "@/lib/constants";
import {
  normalizeToolName,
  isWebSearchToolName,
  isWebFetchToolName,
  parseTerminalToolStartArgs,
} from "@/lib/parsers/toolRequest";

interface ChatInterfaceProps {
  onToggleSidebar: () => void;
  isSidebarOpen: boolean;
}

export function ChatInterface({
  onToggleSidebar,
  isSidebarOpen,
}: ChatInterfaceProps) {
  const navigate = useNavigate();
  const {
    currentSessionId,
    currentProfileId,
    loaded,
    profileSessionsLoading,
    profileAccessBlockedReason,
    profileAccessBlockedCode,
    backendUnavailableReason,
    containerPrepareStatuses,
    profiles,
    models,
    sessions,
    createSession,
    refreshProfileSessions,
    addMessage,
    replaceSessionMessages,
    updateSessionTitle,
    updateSessionModel,
    updateSessionReasoningEffort,
    setSessionPlanState,
    setLastModel,
    uiConfig,
  } = useStore();
  const browserStatus = useStore((state) => state.browserStatus);
  const [inputValue, setInputValue] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleInput, setTitleInput] = useState("");
  const [newChatModelId, setNewChatModelId] = useState("");
  const [newChatReasoningEffort, setNewChatReasoningEffort] = useState<
    string | undefined
  >(undefined);
  const [isSending, setIsSending] = useState(false);
  const [isStreamControllerActive, setIsStreamControllerActive] =
    useState(false);
  const [isModelUpdating, setIsModelUpdating] = useState(false);
  const [uiError, setUiError] = useState<string | null>(null);
  const [isPermissionSubmitting, setIsPermissionSubmitting] = useState(false);
  const [isWorkspaceSubmitting, setIsWorkspaceSubmitting] = useState(false);
  const [pendingAskUser, setPendingAskUser] = useState<AskUserState | null>(
    null,
  );
  const [isAskUserSubmitting, setIsAskUserSubmitting] = useState(false);
  const [isBrowserOpen, setIsBrowserOpen] = useState(false);
  const [artifactPanelWidth, setArtifactPanelWidth] = useState(
    ARTIFACT_PANEL_DEFAULT_WIDTH,
  );
  const [isArtifactPanelResizing, setIsArtifactPanelResizing] = useState(false);
  const [todoProgress, setTodoProgress] = useState<TodoProgressState | null>(null);
  const artifactResizeStartRef = useRef<{
    pointerId: number;
    x: number;
    width: number;
  } | null>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const userScrolledUpRef = useRef(false);
  const pendingInitialScrollRef = useRef(false);
  const composerInputRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);
  const abortControllerRef = useRef<AbortController | null>(null);
  const streamingSessionIdRef = useRef<string | null>(null);
  const stopRequestedSessionIdsRef = useRef<Set<string>>(new Set());
  const stopWaitersRef = useRef<Map<string, Promise<void>>>(new Map());
  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const currentPlanState: SessionPlanState =
    currentSession?.runtime.planState ?? "disabled";
  const currentProfile =
    profiles.find((profile) => profile.id === currentProfileId) ?? null;
  const containerPrepareStatus = currentProfileId
    ? containerPrepareStatuses[currentProfileId]
    : undefined;
  const workspaceSurface = resolveWorkspaceSurfaceState({
    loaded,
    profileSessionsLoading,
    profile: currentProfile,
    status: containerPrepareStatus,
    blockedReason: profileAccessBlockedReason,
    blockedCode: profileAccessBlockedCode,
    backendUnavailableReason,
  });
  const containerPreparation = workspaceSurface.container;
  const isContainerPrepareFailed = containerPreparation.isPrepareFailed;
  const containerPrepareSummary = containerPreparation.summary;
  const requiresManagedPodmanApproval = isManagedPodmanApprovalCode(
    profileAccessBlockedCode,
  );
  const messages = currentSession?.messages || [];
  const hasPendingAssistantMessage = [...messages]
    .reverse()
    .some(
      (message) =>
        message.role === "assistant" &&
        (message.isStreaming || message.isThinking),
    );
  const isNewChat = !currentSessionId || !currentSession;

  const artifactPanel = useArtifactPanel({
    currentSessionId,
    currentProfile,
    messages,
    setError: setUiError,
  });
  const {
    artifacts,
    isOpen: isArtifactsOpen,
    setIsOpen: setIsArtifactsOpen,
    showHistory: showArtifactHistory,
    toggleHistory: toggleArtifactHistory,
    selectedPreviewPath,
    previewPane,
    getViewerForPath: getArtifactViewerHint,
    openPreview,
  } = artifactPanel;
  const handleOpenPreview = useCallback(
    (target: ArtifactPreviewTarget) => {
      void openPreview(target);
    },
    [openPreview],
  );

  const handlePlanStateChange = useCallback(
    async (nextPlanState: SessionPlanState) => {
      if (!currentSessionId) {
        return;
      }
      try {
        // Atomic plan-state change — no fetch-then-save race.
        const result = await apiSetSessionPlanState(
          currentSessionId,
          nextPlanState,
        );
        setSessionPlanState(currentSessionId, result.planState);
      } catch (error) {
        setUiError(error instanceof Error ? error.message : String(error));
      }
    },
    [currentSessionId, setSessionPlanState],
  );
  const shouldShowWelcome = isNewChat || messages.length === 0;
  const chatSurfaceState =
    workspaceSurface.state === "ready"
      ? shouldShowWelcome
        ? "welcome"
        : "chat"
      : workspaceSurface.state;
  const showComposer =
    chatSurfaceState === "welcome" || chatSurfaceState === "chat";
  const hasInput = inputValue.trim().length > 0 || attachments.length > 0;
  const configuredModelIds = (() => {
    const modelConfig = uiConfig.fullConfig?.models;
    if (!modelConfig) {
      return new Set<string>();
    }
    return new Set([
      ...modelConfig.advanced,
      ...modelConfig.balanced,
      ...modelConfig.fast,
    ]);
  })();
  const currentSessionModelId = currentSession?.model ?? "";
  const hasSelectedModel = isNewChat
    ? newChatModelId.trim().length > 0
    : currentSessionModelId.trim().length > 0;
  const isCurrentSessionModelConfigured = isNewChat
    ? true
    : configuredModelIds.size === 0 ||
      configuredModelIds.has(currentSessionModelId);
  const canSend =
    hasInput && hasSelectedModel && isCurrentSessionModelConfigured;
  const showTokenUsage =
    uiConfig.fullConfig?.preferences.showTokenUsage ?? true;
  const latestAssistantWithStats = [...messages]
    .reverse()
    .find((m) => m.role === "assistant" && m.stats);
  const latestStats = latestAssistantWithStats?.stats;
  const contextWindow = latestStats?.contextWindow;
  const totalTokens =
    latestStats?.totalTokens ??
    (typeof latestStats?.promptTokens === "number" &&
    typeof latestStats?.completionTokens === "number"
      ? latestStats.promptTokens + latestStats.completionTokens
      : undefined);
  const contextUsedTokens =
    latestStats?.promptTokens ?? latestStats?.totalTokens ?? totalTokens;
  const contextUsagePercent =
    contextWindow && typeof contextUsedTokens === "number"
      ? Math.max(
          0,
          Math.min(100, Math.round((contextUsedTokens / contextWindow) * 100)),
        )
      : null;
  const contextUsageTitle = latestStats
    ? `Context usage\n${contextUsedTokens ?? "-"}/${contextWindow ?? "-"} tokens (${contextUsagePercent ?? "-"}%)\nPrompt: ${latestStats.promptTokens ?? "-"}\nCompletion: ${latestStats.completionTokens ?? "-"}\nTotal: ${totalTokens ?? "-"}`
    : "No context usage data yet";
  const runtimeSession = useRuntimeStore((state) =>
    currentSessionId ? state.sessions[currentSessionId] : undefined,
  );
  const queuedMessages = runtimeSession
    ? [
        ...(runtimeSession.pendingImmediate
          ? [runtimeSession.pendingImmediate]
          : []),
        ...(runtimeSession.queuedMessages ?? []),
      ]
    : [];
  const pendingPermission: PermissionRequest | null =
    runtimeSession?.waitingPermission ?? null;
  const pendingWorkspace: WorkspaceRequest | null =
    runtimeSession?.waitingWorkspace ?? null;
  const pendingAskUserRequest: AskUserRequest | null =
    runtimeSession?.waitingAskUser ?? null;
  const isGenerating = runtimeSession?.state === "running";
  const runningTerminalCommands = useMemo(() => {
    const deduped = new Map<string, TerminalExecution>();
    for (const message of messages) {
      for (const execution of message.terminalExecutions ?? []) {
        if (execution.status !== "running") {
          continue;
        }
        deduped.set(execution.id, execution);
      }
    }
    return [...deduped.values()].sort(
      (left, right) => (left.startedAt ?? 0) - (right.startedAt ?? 0),
    );
  }, [messages]);
  const backgroundTerminalTask = runtimeSession?.interactions?.terminal
    ? {
        taskId: runtimeSession.interactions.terminal.taskId ?? null,
        outputFile: runtimeSession.interactions.terminal.outputFile ?? null,
        outputPreview: runtimeSession.interactions.terminal.outputPreview ?? null,
        inputNotice: runtimeSession.interactions.terminal.inputNotice ?? null,
      }
    : null;
  const browserOwnedByCurrentSession =
    !!currentSessionId &&
    browserStatus?.ownerSessionId === currentSessionId;
  const browserTakeoverOwnedByCurrentSession =
    !!currentSessionId &&
    browserStatus?.takeoverSessionId === currentSessionId;
  const shouldAutoOpenBrowserPanel =
    !!browserStatus?.running &&
    !!currentSessionId &&
    (browserOwnedByCurrentSession || browserTakeoverOwnedByCurrentSession);
  // Backend runtime state is the source of truth for whether the run is interruptible.
  // Local flags only add transient UI states (sending in flight before runtime snapshot arrives).
  const runtimeInterruptible =
    runtimeSession != null &&
    runtimeSession.state !== 'idle';
  const showStopButton =
    runtimeInterruptible ||
    isGenerating ||
    isSending ||
    isStreamControllerActive ||
    hasPendingAssistantMessage;
  const showComposerSendButton = !showStopButton || hasInput;
  const markRuntimeRunning = useRuntimeStore((state) => state.markRunning);
  const applyRuntime = useRuntimeStore((state) => state.applyRuntime);
  const markRuntimeFirstToken = useRuntimeStore(
    (state) => state.markFirstToken,
  );

  useEffect(() => {
    const persisted = uiConfig.fullConfig?.lastModel;
    if (persisted) {
      if (newChatModelId !== persisted) {
        setNewChatModelId(persisted);
      }
      return;
    }

    const advanced = uiConfig.fullConfig?.models.advanced ?? [];
    const preferredAdvanced = advanced[0] ?? "";

    if (preferredAdvanced) {
      if (newChatModelId !== preferredAdvanced) {
        setNewChatModelId(preferredAdvanced);
      }
      return;
    }

    if (newChatModelId !== "") {
      setNewChatModelId("");
    }
  }, [
    newChatModelId,
    uiConfig.fullConfig?.lastModel,
    uiConfig.fullConfig?.models.advanced,
  ]);

  useEffect(() => {
    if (currentSession) setTitleInput(currentSession.title);
  }, [currentSession?.id, currentSession?.title]);
  useEffect(() => {
    if (isEditingTitle) titleInputRef.current?.focus();
  }, [isEditingTitle]);

  useEffect(() => {
    setUiError(null);
  }, [currentSessionId]);

  useEffect(() => {
    setUiError(null);
    setPendingAskUser(null);
  }, [currentProfileId]);

  useEffect(() => {
    if (!currentSessionId) return;
    useRuntimeStore.getState().ensureSession(currentSessionId);
  }, [currentSessionId]);

  useEffect(() => {
    if (!pendingAskUserRequest) {
      setPendingAskUser(null);
      return;
    }
    setPendingAskUser((previous) => {
      if (previous?.request.id === pendingAskUserRequest.id) {
        return previous;
      }
      return {
        request: pendingAskUserRequest,
        answers: pendingAskUserRequest.questions.map((question) => ({
          section: question.section,
          answer: { kind: "skipped" },
        })),
      };
    });
  }, [pendingAskUserRequest]);

  useEffect(() => {
    if (!currentSessionId) {
      setPendingAskUser(null);
      return;
    }
  }, [currentSessionId]);

  useEffect(() => {
    userScrolledUpRef.current = false;
    pendingInitialScrollRef.current = currentSessionId != null;
  }, [currentSessionId]);

  // Auto-scroll: follow output unless user intentionally scrolled up.
  const contentRef = useRef<HTMLDivElement>(null);

  const scrollToBottom = () => {
    const container = scrollContainerRef.current;
    if (!container) return;
    container.scrollTop = container.scrollHeight;
  };

  useEffect(() => {
    if (
      !currentSessionId ||
      chatSurfaceState !== "chat" ||
      !pendingInitialScrollRef.current
    ) {
      return;
    }

    let rafA = 0;
    let rafB = 0;
    rafA = requestAnimationFrame(() => {
      scrollToBottom();
      rafB = requestAnimationFrame(() => {
        scrollToBottom();
        userScrolledUpRef.current = false;
        pendingInitialScrollRef.current = false;
      });
    });

    return () => {
      if (rafA) {
        cancelAnimationFrame(rafA);
      }
      if (rafB) {
        cancelAnimationFrame(rafB);
      }
    };
  }, [chatSurfaceState, currentSessionId, messages.length]);

  useEffect(() => {
    const container = scrollContainerRef.current;
    const content = contentRef.current;
    if (!container || !content) return;

    const observer = new ResizeObserver(() => {
      if (userScrolledUpRef.current) return;
      scrollToBottom();
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [currentSessionId]);

  useEffect(() => {
    if (!currentSessionId || messages.length === 0) {
      return;
    }
    if (isGenerating || isSending || isStreamControllerActive) {
      return;
    }

    const settledAt = Date.now();
    let changed = false;
    const nextMessages = messages.map((message) => {
      if (message.role !== "assistant") {
        return message;
      }

      const timeline = message.timelineSegments ?? [];
      const hasRunningTimeline = timeline.some((segment) =>
        segment.kind === "thinking"
          ? segment.isRunning
          : segment.kind === "terminal"
            ? segment.commands.some((command) => command.status === "running")
            : segment.tool.status === "running",
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
  }, [
    currentSessionId,
    isGenerating,
    isSending,
    isStreamControllerActive,
    messages,
    replaceSessionMessages,
  ]);

  const lastMessage = messages[messages.length - 1];
  useEffect(() => {
    if (userScrolledUpRef.current) return;
    requestAnimationFrame(scrollToBottom);
  }, [
    currentSessionId,
    messages.length,
    lastMessage?.content,
    lastMessage?.thinking,
    lastMessage?.isStreaming,
    lastMessage?.isThinking,
  ]);

  const handleScroll = () => {
    if (pendingInitialScrollRef.current) {
      return;
    }
    const el = scrollContainerRef.current;
    if (!el) return;
    const distFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (distFromBottom > 150) {
      userScrolledUpRef.current = true;
    } else if (distFromBottom < 30) {
      userScrolledUpRef.current = false;
    }
  };

  const removeQueuedMessage = useCallback(
    async (index: number) => {
      if (!currentSessionId) {
        return;
      }
      try {
        const hasPendingImmediate = runtimeSession?.pendingImmediate != null;
        const kind =
          hasPendingImmediate && index === 0 ? "pending_immediate" : "queued";
        const queuedIndex = hasPendingImmediate ? index - 1 : index;
        const runtime = await removePendingSessionMessage({
          sessionId: currentSessionId,
          kind,
          index: kind === "queued" ? queuedIndex : undefined,
        });
        applyRuntime(currentSessionId, runtime);
      } catch (error) {
        setUiError(
          `Failed to remove queued message: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      }
    },
    [applyRuntime, currentSessionId, runtimeSession],
  );

  const steerQueuedMessage = useCallback(
    (index: number) => {
      const queued = queuedMessages[index];
      if (!queued) {
        return;
      }

      setInputValue((prev) =>
        prev.trim().length > 0
          ? `${queued.content}\n\n${prev}`
          : queued.content,
      );

      if (queued.attachments.length > 0) {
        const timestamp = Date.now();
        const restored = queued.attachments.map(
          (attachment, attachmentIndex) => ({
            ...attachment,
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
    [queuedMessages, removeQueuedMessage],
  );

  const handleModelChange = (id: string) => {
    if (isNewChat) {
      setNewChatModelId(id);
      setIsModelUpdating(true);
      void setLastModel(id)
        .catch((error) => {
          setUiError(
            `Failed to update default model: ${
              error instanceof Error ? error.message : String(error)
            }`,
          );
        })
        .finally(() => {
          setIsModelUpdating(false);
        });
      return;
    }
    if (!currentSessionId) {
      return;
    }
    setIsModelUpdating(true);
    void updateSessionModel(currentSessionId, id)
      .catch((error) => {
        setUiError(
          `Failed to update session model: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      })
      .finally(() => {
        setIsModelUpdating(false);
      });
  };

  const handleReasoningEffortChange = (effort: string | undefined) => {
    if (isNewChat) {
      setNewChatReasoningEffort(effort);
    } else if (currentSessionId) {
      updateSessionReasoningEffort(currentSessionId, effort);
    }
  };

  const handleRemoveAttachment = (id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  const composerPlaceholder =
    queuedMessages.length > 0
      ? "Ask for follow-up changes"
      : isNewChat
        ? "How can I help you today?"
        : "Type a message...";

  const syncSessionRuntime = useCallback(
    async (sessionId: string) => {
      const runtime = await fetchSessionRuntime(sessionId);
      applyRuntime(sessionId, runtime);
      return runtime;
    },
    [applyRuntime],
  );

  const waitForStoppedSession = useCallback(
    async (sessionId: string) => {
      const runtime = await stopSessionRun(sessionId);
      applyRuntime(sessionId, runtime);
    },
    [applyRuntime],
  );

  const handleSend = async (options?: {
    sessionIdHint?: string;
    includeComposerInput?: boolean;
    retryAssistantId?: string;
    overrideBatch?: QueuedMessage[];
    skipAppendUserMessages?: boolean;
    deliveryMode?: "steer" | "queue";
  }) => {
    const includeComposerInput = options?.includeComposerInput ?? true;
    const composerHasInput =
      inputValue.trim().length > 0 || attachments.length > 0;
    if (includeComposerInput && !composerHasInput) return;

    const requestedSessionId = options?.sessionIdHint ?? currentSessionId;
    const pendingStop = requestedSessionId
      ? stopWaitersRef.current.get(requestedSessionId)
      : undefined;
    if (pendingStop) {
      try {
        await pendingStop;
      } catch (error) {
        setUiError(
          `Failed to stop session: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
        return;
      }
    }
    if (!requestedSessionId) {
      const validation = validateModelConfigured(
        newChatModelId,
        configuredModelIds,
        "No model configured. Please select a model before sending.",
        "Selected model is not available in the current profile.",
      );
      if (!validation.ok) {
        setUiError(validation.error!);
        return;
      }
    } else {
      const targetSession = useStore
        .getState()
        .sessions.find((session) => session.id === requestedSessionId);
      const validation = validateModelConfigured(
        targetSession?.model,
        configuredModelIds,
        "No model configured for this session. Please select a model.",
        "Session model is no longer available. Please choose another model.",
      );
      if (!validation.ok) {
        setUiError(validation.error!);
        return;
      }
    }

    setIsSending(true);
    setUiError(null);

    const content = includeComposerInput ? inputValue : "";
    const atts = includeComposerInput ? [...attachments] : [];
    if (includeComposerInput) {
      setInputValue("");
      setAttachments([]);
    }

    const targetRuntime = options?.sessionIdHint
      ? useRuntimeStore.getState().sessions[options.sessionIdHint]
      : runtimeSession;
    const targetGenerating = !!targetRuntime && isRuntimeTargetBusy(targetRuntime);
    const deliveryMode =
      options?.deliveryMode ?? (targetGenerating ? "queue" : "steer");

    if (targetGenerating) {
      if (!includeComposerInput) {
        setIsSending(false);
        return;
      }
      const runtimeSessionId = options?.sessionIdHint ?? currentSessionId;
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
          setUiError(message);
          // Restore the composer draft so the user can retry.
          setInputValue(content);
          setAttachments(atts);
        }
      }
      setIsSending(false);
      return;
    }

    let sessionId = options?.sessionIdHint ?? currentSessionId;
    if (!sessionId) {
      try {
        sessionId = await createSession(newChatModelId || undefined);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        setUiError(message);
        // Restore draft so user can retry after fixing the issue.
        if (includeComposerInput) {
          setInputValue(content);
          setAttachments(atts);
        }
        setIsSending(false);
        return;
      }
      if (sessionId) {
        navigate(`/chat/${encodeURIComponent(sessionId)}`);
      }
    }

    if (!sessionId) {
      setIsSending(false);
      return;
    }

    stopRequestedSessionIdsRef.current.delete(sessionId);

    const resolvedSessionModelId =
      useStore
        .getState()
        .sessions.find((session) => session.id === sessionId)
        ?.model?.trim() ?? "";
    if (!resolvedSessionModelId) {
      setIsSending(false);
      setUiError(
        "No model configured for this session. Please select a model.",
      );
      return;
    }
    if (
      configuredModelIds.size > 0 &&
      !configuredModelIds.has(resolvedSessionModelId)
    ) {
      setIsSending(false);
      setUiError(
        "Session model is no longer available. Please choose another model.",
      );
      return;
    }

    const pendingBatch = options?.overrideBatch
      ? [...options.overrideBatch]
      : includeComposerInput
        ? [{ content, attachments: atts }]
        : [];

    if (pendingBatch.length === 0) {
      setIsSending(false);
      return;
    }

    const baseMessages =
      useStore.getState().sessions.find((session) => session.id === sessionId)
        ?.messages ?? [];

    if (!options?.skipAppendUserMessages) {
      for (const queued of pendingBatch) {
        addMessage(sessionId, queued.content, "user", queued.attachments);
      }
    }

    userScrolledUpRef.current = false;
    streamingSessionIdRef.current = sessionId;
    markRuntimeRunning(sessionId);

    const controller = new AbortController();
    abortControllerRef.current = controller;
    setIsStreamControllerActive(true);
    let completedNormally = false;
    const assistantId = options?.retryAssistantId ?? `assistant-${Date.now()}`;
    let textFlushTimer: number | null = null;
    let streamedText = "";
    let timelineSegments: TimelineSegment[] = [];
    setTodoProgress(null);
    let activeThinkingSegmentId: string | null = null;
    const terminalSegmentByToolId = new Map<string, string>();
    const toolSegmentByToolId = new Map<string, string>();
    let lastStreamEventKind: "reasoning" | "text" | "tool" | null = null;
    let lastTextFlushAt = 0;
    const wasStoppedByUser = (): boolean =>
      controller.signal.aborted ||
      stopRequestedSessionIdsRef.current.has(sessionId);

    const isActiveSessionStreamFailure = (error: unknown): boolean =>
      isSessionActiveConflictError(error);

    const restoreComposerDraft = () => {
      if (!includeComposerInput || options?.retryAssistantId || pendingBatch.length !== 1) {
        return;
      }
      const [draft] = pendingBatch;
      if (!draft) {
        return;
      }
      setInputValue(draft.content);
      setAttachments(draft.attachments);
    };

    const rollbackOptimisticStreamStart = async (message: string) => {
      replaceSessionMessages(sessionId, baseMessages);
      restoreComposerDraft();
      try {
        const runtime = await fetchSessionRuntime(sessionId);
        applyRuntime(sessionId, runtime);
      } catch (runtimeError) {
        setUiError(
          `Failed to refresh session state: ${
            runtimeError instanceof Error
              ? runtimeError.message
              : String(runtimeError)
          }`,
        );
        return;
      }
      setUiError(message);
    };

    const closeActiveThinkingSegment = () => {
      if (!activeThinkingSegmentId) {
        return;
      }
      timelineSegments = closeThinkingSegment(
        timelineSegments,
        activeThinkingSegmentId,
        Date.now(),
      );
      activeThinkingSegmentId = null;
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

      const finalPrompt = streamingBatch.map((m) => m.content).join("\n\n");
      const mergedAttachments = streamingBatch.flatMap((m) => m.attachments);

      if (options?.retryAssistantId) {
        replaceSessionMessages(
          sessionId,
          current.messages.map((m) =>
            m.id === assistantId
              ? {
                  ...m,
                  content: "",
                  error: undefined,
                  isStreaming: true,
                  isThinking: true,
                  thinking: "",
                  timelineSegments: [],
                  terminalExecutions: [],
                }
              : m,
          ),
        );
      } else {
        replaceSessionMessages(sessionId, [
          ...current.messages,
          {
            id: assistantId,
            role: "assistant",
            content: "",
            timestamp: new Date().toISOString(),
            attachments: [],
            isStreaming: true,
            isThinking: true,
            thinking: "",
            timelineSegments: [],
            terminalExecutions: [],
          },
        ]);
      }

      const flushAssistantText = (force = false) => {
        const now = Date.now();
        const minIntervalMs = 50;
        const elapsed = now - lastTextFlushAt;

        if (!force && elapsed < minIntervalMs) {
          if (textFlushTimer == null) {
            textFlushTimer = window.setTimeout(() => {
              textFlushTimer = null;
              flushAssistantText(true);
            }, minIntervalMs - elapsed);
          }
          return;
        }

        const latest = useStore
          .getState()
          .sessions.find((s) => s.id === sessionId);
        if (!latest) return;

        lastTextFlushAt = now;
        const snapshotTimelineSegments =
          cloneTimelineSegments(timelineSegments);
        const snapshotTerminalExecutions = collectTerminalExecutions(
          snapshotTimelineSegments,
        );
        const snapshotThinking = collectThinkingText(snapshotTimelineSegments);
        replaceSessionMessages(
          sessionId,
          latest.messages.map((m) =>
            m.id === assistantId
              ? {
                  ...m,
                  content: streamedText,
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

      await streamChat({
        sessionId,
        content: finalPrompt,
        attachments: mergedAttachments,
        signal: controller.signal,
        onReasoning: (chunk) => {
          const now = Date.now();
          const shouldCreateSegment =
            !activeThinkingSegmentId || lastStreamEventKind !== "reasoning";
          if (shouldCreateSegment) {
            const nextId = `thinking-${now}-${Math.random().toString(36).slice(2, 8)}`;
            activeThinkingSegmentId = nextId;
            timelineSegments = [
              ...timelineSegments,
              {
                id: nextId,
                kind: "thinking",
                offset: streamedText.length,
                content: chunk,
                isRunning: true,
                startedAt: now,
              },
            ];
          } else {
            timelineSegments = timelineSegments.map((segment) =>
              segment.kind === "thinking" &&
              segment.id === activeThinkingSegmentId
                ? {
                    ...segment,
                    content: `${segment.content}${chunk}`,
                    isRunning: true,
                  }
                : segment,
            );
          }
          lastStreamEventKind = "reasoning";
          flushAssistantText(true);
        },
        onToolStart: (tool: any) => {
          const name = normalizeToolName(tool.name);
          const toolId = tool.id ?? `tool-${Date.now()}`;
          const serializedArguments =
            typeof tool.arguments === "string"
              ? tool.arguments
              : JSON.stringify(tool.arguments ?? {});

          if (name === "terminal") {
            const args = parseTerminalToolStartArgs(tool.arguments);
            closeActiveThinkingSegment();
            const startedAt = Date.now();
            const nextExec: TerminalExecution = {
              id: toolId,
              description: args.description,
              input: args.script,
              status: "running",
              mode: args.mode ?? "default",
              startedAt,
            };

            const lastSegment = timelineSegments[timelineSegments.length - 1];
            const canAppendToBatch =
              lastStreamEventKind === "tool" &&
              lastSegment?.kind === "terminal" &&
              lastSegment.offset === streamedText.length;

            let segmentId: string;
            if (canAppendToBatch && lastSegment.kind === "terminal") {
              segmentId = lastSegment.id;
              timelineSegments = timelineSegments.map((segment) =>
                segment.kind === "terminal" && segment.id === segmentId
                  ? {
                      ...segment,
                      startedAt: segment.startedAt ?? startedAt,
                      endedAt: undefined,
                      commands: [...segment.commands, nextExec],
                    }
                  : segment,
              );
            } else {
              segmentId = `terminal-${toolId}`;
              timelineSegments = [
                ...timelineSegments,
                {
                  id: segmentId,
                  kind: "terminal",
                  offset: streamedText.length,
                  commands: [nextExec],
                  startedAt,
                },
              ];
            }

            terminalSegmentByToolId.set(toolId, segmentId);
            lastStreamEventKind = "tool";
            flushAssistantText(true);
            return;
          }

          if (isWebSearchToolName(name) || isWebFetchToolName(name)) {
            closeActiveThinkingSegment();
            const startedAt = Date.now();
            const nextTool: ToolExecution = {
              id: toolId,
              name,
              request: serializedArguments,
              status: "running",
              startedAt,
            };
            const segmentId = `tool-${toolId}`;
            timelineSegments = [
              ...timelineSegments,
              {
                id: segmentId,
                kind: "tool",
                offset: streamedText.length,
                tool: nextTool,
              },
            ];
            toolSegmentByToolId.set(toolId, segmentId);
            lastStreamEventKind = "tool";
            flushAssistantText(true);
          }
        },
        onToolEnd: (tool: any) => {
          const toolId = typeof tool?.id === "string" ? tool.id : "";
          if (!toolId) return;
          const serializedArguments =
            typeof tool?.arguments === "string"
              ? tool.arguments
              : JSON.stringify(tool?.arguments ?? {});
          const endedAt = Date.now();
          const { output, status } = parseToolResultPayload(tool.result);
          const terminalSegmentId = terminalSegmentByToolId.get(toolId);
          if (terminalSegmentId) {
            timelineSegments = timelineSegments.map((segment) => {
              if (segment.kind !== "terminal" || segment.id !== terminalSegmentId) {
                return segment;
              }
              const nextCommands = segment.commands.map((command) =>
                command.id === toolId
                  ? {
                      ...command,
                      output,
                      status,
                      endedAt,
                    }
                  : command,
              );
              const runningLeft = nextCommands.some(
                (command) => command.status === "running",
              );
              return {
                ...segment,
                commands: nextCommands,
                endedAt: runningLeft ? undefined : endedAt,
              };
            });
            lastStreamEventKind = "tool";
            flushAssistantText(true);
            return;
          }

          const toolSegmentId = toolSegmentByToolId.get(toolId);
          if (!toolSegmentId) {
            return;
          }
          timelineSegments = timelineSegments.map((segment) =>
            segment.kind === "tool" && segment.id === toolSegmentId
              ? {
                  ...segment,
                  tool: {
                    ...segment.tool,
                    request: segment.tool.request ?? serializedArguments,
                    response: output,
                    status,
                    endedAt,
                  },
                }
              : segment,
          );
          lastStreamEventKind = "tool";
          flushAssistantText(true);
        },
        onTodoUpdate: (event) => {
          setTodoProgress(event);
        },
        onText: (chunk) => {
          closeActiveThinkingSegment();
          streamedText += chunk;
          lastStreamEventKind = "text";
          markRuntimeFirstToken(sessionId);
          flushAssistantText();
        },
        onDone: (assistant) => {
          closeActiveThinkingSegment();
          if (textFlushTimer != null) {
            window.clearTimeout(textFlushTimer);
            textFlushTimer = null;
          }
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          const finalContent =
            streamedText.trim().length > 0 ? streamedText : assistant.content;
          const finalTimelineSegments =
            timelineSegments.length > 0
              ? cloneTimelineSegments(timelineSegments)
              : cloneTimelineSegments(assistant.timelineSegments ?? []);
          const finalThinking = collectThinkingText(finalTimelineSegments);
          const finalTerminalExecutions = collectTerminalExecutions(
            finalTimelineSegments,
          );
          const isEmptyResponse =
            finalContent.trim().length === 0 &&
            finalThinking.trim().length === 0 &&
            finalTimelineSegments.length === 0;
          completedNormally = !isEmptyResponse;

          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...assistant,
                    id: assistantId,
                    content: finalContent,
                    thinking: finalThinking,
                    timelineSegments: finalTimelineSegments,
                    isStreaming: false,
                    isThinking: false,
                    terminalExecutions: finalTerminalExecutions,
                    error: isEmptyResponse
                      ? "LLM endpoint error: empty assistant response"
                      : undefined,
                  }
                : m,
            ),
          );
        },
        onPaused: (assistant) => {
          closeActiveThinkingSegment();
          if (textFlushTimer != null) {
            window.clearTimeout(textFlushTimer);
            textFlushTimer = null;
          }
          completedNormally = true;
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) {
            return;
          }
          const finalTimelineSegments = cloneTimelineSegments(timelineSegments);
          const finalThinking = collectThinkingText(finalTimelineSegments);
          const finalTerminalExecutions = collectTerminalExecutions(
            finalTimelineSegments,
          );
          const isEmptyPause =
            assistant.content.trim().length === 0 &&
            finalThinking.trim().length === 0 &&
            finalTimelineSegments.length === 0;

          if (isEmptyPause) {
            replaceSessionMessages(
              sessionId,
              latest.messages.filter((m) => m.id !== assistantId),
            );
            return;
          }

          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...assistant,
                    id: assistantId,
                    content: assistant.content,
                    thinking: finalThinking,
                    timelineSegments: finalTimelineSegments,
                    isStreaming: false,
                    isThinking: false,
                    terminalExecutions: finalTerminalExecutions,
                    error: undefined,
                  }
                : m,
            ),
          );
        },
        onError: (message) => {
          if (wasStoppedByUser()) {
            completedNormally = true;
            return;
          }
          closeActiveThinkingSegment();
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (latest) {
            const snapshotTimelineSegments =
              cloneTimelineSegments(timelineSegments);
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
                      error: `LLM endpoint error: ${message}`,
                    }
                  : m,
              ),
            );
          }
          setUiError(`LLM endpoint error: ${message}`);
        },
      });
    } catch (error) {
      if (wasStoppedByUser()) {
        completedNormally = true;
        return;
      }
      if (!(error instanceof DOMException && error.name === "AbortError")) {
        if (isActiveSessionStreamFailure(error)) {
          await rollbackOptimisticStreamStart(
            "This session is still active. Wait for the current run to settle or stop it before sending another message.",
          );
          return;
        }
        if (isBrowserTakeoverActiveError(error)) {
          await rollbackOptimisticStreamStart(
            "Browser takeover is active for this session. Return browser control before sending another message.",
          );
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        closeActiveThinkingSegment();
        const errText = `LLM endpoint error: ${message}`;
        const latest = useStore
          .getState()
          .sessions.find((s) => s.id === sessionId);
        if (latest) {
          const snapshotTimelineSegments =
            cloneTimelineSegments(timelineSegments);
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
                    error: errText,
                  }
                : m,
            ),
          );
        }
        setUiError(errText);
      }
    } finally {
      if (textFlushTimer != null) {
        window.clearTimeout(textFlushTimer);
        textFlushTimer = null;
      }

      const latest = useStore
        .getState()
        .sessions.find((s) => s.id === sessionId);
      if (latest) {
        const stopRequestedByUser =
          stopRequestedSessionIdsRef.current.has(sessionId);
        const assistantStopped =
          latest.messages.some(
            (message) =>
              message.id === assistantId &&
              timelineContainsStoppedWork(message.timelineSegments ?? []),
          );
        const terminalError = !completedNormally && !stopRequestedByUser && !assistantStopped
          ? "LLM endpoint error: stream terminated before a final assistant response"
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

      abortControllerRef.current = null;
      setIsStreamControllerActive(false);
      setIsSending(false);
      if (streamingSessionIdRef.current === sessionId) {
        streamingSessionIdRef.current = null;
      }
      setIsBrowserOpen(false);

      void syncSessionRuntime(sessionId)
        .catch((error) => {
          setUiError(
            `Failed to sync runtime state: ${
              error instanceof Error ? error.message : String(error)
            }`,
          );
        })
        .finally(() => {
          stopRequestedSessionIdsRef.current.delete(sessionId);
        });
    }
  };

  const handleStop = () => {
    const runtimeSessionId = streamingSessionIdRef.current ?? currentSessionId;
    if (!runtimeSessionId) {
      return;
    }
    const ownsLocalStream = streamingSessionIdRef.current === runtimeSessionId;
    stopRequestedSessionIdsRef.current.add(runtimeSessionId);
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    setIsStreamControllerActive(false);
    setIsSending(false);
    setIsBrowserOpen(false);
    const waitForStop = waitForStoppedSession(runtimeSessionId).finally(() => {
      stopWaitersRef.current.delete(runtimeSessionId);
      if (!ownsLocalStream) {
        stopRequestedSessionIdsRef.current.delete(runtimeSessionId);
      }
    });
    stopWaitersRef.current.set(runtimeSessionId, waitForStop);
    void waitForStop.catch((error) => {
      setUiError(
        `Failed to stop session: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    });
    const latest = useStore
      .getState()
      .sessions.find((s) => s.id === runtimeSessionId);
    if (latest) {
      let stopped = false;
      const endedAt = Date.now();
      replaceSessionMessages(
        runtimeSessionId,
        latest.messages.map((m) => {
          if (
            !stopped &&
            m.role === "assistant" &&
            (m.isStreaming || m.isThinking)
          ) {
            stopped = true;
            const timelineSegments = settleTimelineSegments(
              m.timelineSegments,
              endedAt,
              "stopped",
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
    streamingSessionIdRef.current = null;
  };

  const handlePermissionDecision = async (approved: boolean) => {
    if (!pendingPermission || !currentSessionId) return;
    setIsPermissionSubmitting(true);
    try {
      const runtime = await respondPermissionRequest(
        pendingPermission.id,
        approved,
      );
      applyRuntime(currentSessionId, runtime);
    } catch (error) {
      setUiError(
        `permission submit failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    } finally {
      setIsPermissionSubmitting(false);
    }
  };

  const handleWorkspaceDecision = async (approved: boolean) => {
    if (!pendingWorkspace || !currentSessionId) return;
    setIsWorkspaceSubmitting(true);
    try {
      const runtime = await respondWorkspaceRequest(
        pendingWorkspace.id,
        approved,
      );
      applyRuntime(currentSessionId, runtime);
    } catch (error) {
      setUiError(
        `workspace submit failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    } finally {
      setIsWorkspaceSubmitting(false);
    }
  };

  const handleAskUserAnswerChange = (
    questionIndex: number,
    value: AskUserAnswer["answer"],
  ) => {
    setPendingAskUser((prev) => {
      if (!prev) return prev;
      const nextAnswers = [...prev.answers];
      nextAnswers[questionIndex] = {
        section: prev.request.questions[questionIndex]?.section ?? "",
        answer: value,
      };
      return { ...prev, answers: nextAnswers };
    });
  };

  const handleAskUserSubmit = useCallback(
    async (answersOverride?: AskUserAnswer[]) => {
      if (!pendingAskUser || !currentSessionId) return;
      setIsAskUserSubmitting(true);
      setUiError(null);
      try {
        const runtime = await respondAskUserRequest(
          pendingAskUser.request.id,
          answersOverride ?? pendingAskUser.answers,
        );
        applyRuntime(currentSessionId, runtime);
        setPendingAskUser(null);
      } catch (error) {
        setUiError(`ask_user submit failed: ${(error as Error).message}`);
      } finally {
        setIsAskUserSubmitting(false);
      }
    },
    [applyRuntime, currentSessionId, pendingAskUser],
  );

  const handleTitleSubmit = async () => {
    const trimmed = titleInput.trim();
    if (!currentSessionId || !trimmed) {
      setIsEditingTitle(false);
      return;
    }
    try {
      await updateSessionTitle(currentSessionId, trimmed);
      setIsEditingTitle(false);
    } catch (error) {
      setUiError(
        error instanceof Error ? error.message : 'Failed to update title',
      );
    }
  };

  const handleScreenshot = async () => {
    let stream: MediaStream | null = null;
    try {
      stream = await navigator.mediaDevices.getDisplayMedia({
        video: { displaySurface: "browser" },
        audio: false,
        preferCurrentTab: true,
      } as DisplayMediaStreamOptions);
      const video = document.createElement("video");
      video.srcObject = stream;
      await new Promise((r) => {
        video.onloadedmetadata = () => video.play().then(r);
      });
      const canvas = document.createElement("canvas");
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      canvas.getContext("2d")?.drawImage(video, 0, 0);
      setAttachments((prev) => [
        ...prev,
        {
          id: crypto.randomUUID(),
          name: `screenshot-${Date.now()}.png`,
          type: "image",
          url: canvas.toDataURL("image/png"),
        },
      ]);
    } catch (e) {
      setUiError(
        `Screenshot failed: ${e instanceof Error ? e.message : String(e)}`,
      );
    } finally {
      stream?.getTracks().forEach((t) => t.stop());
    }
  };

  const handleFileSelect = async (e: React.ChangeEvent<HTMLInputElement>) => {
    if (!e.target.files?.length) return;
    try {
      const files = Array.from(e.target.files!);
      const newAttachments = await Promise.all(files.map(buildAttachmentFromFile));
      setAttachments((prev) => [...prev, ...newAttachments]);
      if (fileInputRef.current) fileInputRef.current.value = "";
    } catch (error) {
      setUiError(
        `Attachment processing failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
  };

  const handlePaste = async (e: React.ClipboardEvent) => {
    const items = Array.from(e.clipboardData.items);
    const text = e.clipboardData.getData("text/plain");
    const hasFileItem = items.some(
      (item) => item.kind === "file" && item.getAsFile(),
    );
    const shouldAttachLargeText =
      !hasFileItem && !!text && text.length >= LONG_TEXT_THRESHOLD;

    // Prevent default immediately, before async work, so filename/text is never inserted into composer.
    if (hasFileItem || shouldAttachLargeText) {
      e.preventDefault();
    }

    const fileItems = items
      .filter((item) => item.kind === "file")
      .map((item) => item.getAsFile())
      .filter((file): file is File => file != null);

    const newAttachments = await Promise.all(
      fileItems.map(buildAttachmentFromPastedFile),
    );

    if (shouldAttachLargeText) {
      newAttachments.push(buildAttachmentFromPastedText(text));
    }

    if (newAttachments.length > 0) {
      setAttachments((prev) => [...prev, ...newAttachments]);
    }
  };

  const currentModelId = isNewChat ? newChatModelId : currentSession.model;
  const currentReasoningEffort = isNewChat
    ? newChatReasoningEffort
    : currentSession.reasoningEffort;
  const currentModelSupportsReasoning =
    (currentModelId.trim().length > 0
      ? getModel(currentModelId, models)?.reasoning
      : false) ?? false;

  const canShowArtifactsPanel = artifacts.length > 0 || !!selectedPreviewPath;
  const activeRightPanel = isBrowserOpen || shouldAutoOpenBrowserPanel
    ? "browser"
    : isArtifactsOpen && canShowArtifactsPanel
      ? "artifacts"
      : null;

  const clampArtifactPanelWidth = useCallback(
    (rawWidth: number): number => {
      if (typeof window === "undefined") {
        return rawWidth;
      }
      const viewportWidth = window.innerWidth;
      const desktopSidebarWidth = isSidebarOpen ? 256 : 0;
      const maxByViewport = Math.floor(
        viewportWidth * ARTIFACT_PANEL_MAX_VIEWPORT_RATIO,
      );
      const maxByChat =
        viewportWidth - desktopSidebarWidth - ARTIFACT_PANEL_MIN_CHAT_WIDTH;
      const maxWidth = Math.max(
        ARTIFACT_PANEL_MIN_WIDTH,
        Math.min(maxByViewport, maxByChat),
      );
      return Math.max(
        ARTIFACT_PANEL_MIN_WIDTH,
        Math.min(maxWidth, Math.round(rawWidth)),
      );
    },
    [isSidebarOpen],
  );

  const handleArtifactResizerPointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      event.preventDefault();
      artifactResizeStartRef.current = {
        pointerId: event.pointerId,
        x: event.clientX,
        width: artifactPanelWidth,
      };
      setIsArtifactPanelResizing(true);
    },
    [artifactPanelWidth],
  );

  useEffect(() => {
    const updateWidth = () => {
      setArtifactPanelWidth((prev) => clampArtifactPanelWidth(prev));
    };
    updateWidth();
    window.addEventListener("resize", updateWidth);
    return () => {
      window.removeEventListener("resize", updateWidth);
    };
  }, [clampArtifactPanelWidth]);

  useEffect(() => {
    if (!isArtifactPanelResizing) return;

    const previousUserSelect = document.body.style.userSelect;
    const previousCursor = document.body.style.cursor;
    document.body.style.userSelect = "none";
    document.body.style.cursor = "col-resize";

    const handlePointerMove = (event: PointerEvent) => {
      const resizeState = artifactResizeStartRef.current;
      if (!resizeState || event.pointerId !== resizeState.pointerId) {
        return;
      }
      if ((event.buttons & 1) === 0) {
        artifactResizeStartRef.current = null;
        setIsArtifactPanelResizing(false);
        return;
      }
      const delta = resizeState.x - event.clientX;
      setArtifactPanelWidth(clampArtifactPanelWidth(resizeState.width + delta));
    };

    const stopResizing = (event: PointerEvent) => {
      const resizeState = artifactResizeStartRef.current;
      if (!resizeState || event.pointerId !== resizeState.pointerId) {
        return;
      }
      artifactResizeStartRef.current = null;
      setIsArtifactPanelResizing(false);
    };

    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", stopResizing);
    window.addEventListener("pointercancel", stopResizing);

    return () => {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", stopResizing);
      window.removeEventListener("pointercancel", stopResizing);
      document.body.style.userSelect = previousUserSelect;
      document.body.style.cursor = previousCursor;
    };
  }, [clampArtifactPanelWidth, isArtifactPanelResizing]);

  if (chatSurfaceState === "backend-down") {
    return (
      <div className="flex flex-1 min-h-0 w-full min-w-0 items-center justify-center overflow-hidden bg-[#8f2020] px-6 py-10 text-red-50 md:px-10">
        <div className="w-full max-w-3xl text-center">
          <div className="mx-auto mb-6 flex h-16 w-16 items-center justify-center rounded-full border border-red-50/18">
            <CircleX className="h-8 w-8 text-red-50/92" />
          </div>
          <h2 className="text-3xl font-semibold tracking-[-0.03em] text-red-50">
            Backend unavailable
          </h2>
          <p className="mx-auto mt-4 max-w-xl text-base leading-7 text-red-50/76">
            The UI cannot reach the May server right now.
          </p>
          <div className="mx-auto mt-8 max-w-2xl rounded-3xl bg-red-950/16 px-6 py-4 text-sm leading-7 text-red-50/78 shadow-[inset_0_1px_0_rgba(255,255,255,0.06)] backdrop-blur-sm">
            {backendUnavailableReason}
          </div>
          <Button
            variant="secondary"
            className="mt-8 h-10 rounded-full border-0 bg-red-50 text-[#8f2020] hover:bg-white"
            onClick={() => void refreshProfileSessions()}
          >
            Retry
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 min-h-0 w-full min-w-0">
      <div className="relative flex flex-1 min-h-0 min-w-0 flex-col">
        <ChatHeader
          isSidebarOpen={isSidebarOpen}
          onToggleSidebar={onToggleSidebar}
          isNewChat={isNewChat}
          shouldShowWelcome={shouldShowWelcome}
          sessionTitle={currentSession?.title ?? ''}
          isEditingTitle={isEditingTitle}
          titleInput={titleInput}
          titleInputRef={titleInputRef}
          onTitleInputChange={setTitleInput}
          onBeginEdit={() => setIsEditingTitle(true)}
          onTitleSubmit={() => void handleTitleSubmit()}
          onTitleCancel={() => {
            if (currentSession) {
              setTitleInput(currentSession.title);
            }
            setIsEditingTitle(false);
          }}
          planState={currentPlanState}
          onPlanStateToggle={() => {
            const nextPlanState =
              currentPlanState === "disabled"
                ? "draft"
                : currentPlanState === "draft"
                  ? "approved"
                  : "draft";
            void handlePlanStateChange(nextPlanState);
          }}
        />

        {chatSurfaceState === "container-preparing" ? (
          <div className="flex-1 min-h-0 px-4 pt-12">
            <div className="mx-auto w-full max-w-3xl rounded-2xl border border-border/50 bg-background/70 p-6 text-center">
              {isContainerPrepareFailed ? (
                <div className="mx-auto mb-4 flex w-fit items-center justify-center rounded-full bg-red-500/15 p-2.5">
                  <CircleX className="h-6 w-6 text-red-500" />
                </div>
              ) : (
                <div className="mx-auto mb-4 flex w-fit items-center gap-2">
                  <span className="h-2.5 w-2.5 rounded-full bg-primary/70 animate-bounce [animation-delay:-240ms]" />
                  <span className="h-2.5 w-2.5 rounded-full bg-primary/60 animate-bounce [animation-delay:-120ms]" />
                  <span className="h-2.5 w-2.5 rounded-full bg-primary/50 animate-bounce" />
                </div>
              )}
              <h2 className="text-lg font-semibold">
                {isContainerPrepareFailed
                  ? "Container workspace unavailable"
                  : "Preparing container workspace"}
              </h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {isContainerPrepareFailed
                  ? "Unable to open chat history right now. Fix the reported error and retry."
                  : "This profile is not ready yet. New chat and chat history are temporarily unavailable."}
              </p>
              <div className="mt-4 rounded-lg bg-muted/50 px-3 py-2 text-left text-xs text-muted-foreground">
                {containerPrepareSummary}
              </div>
              {containerPrepareStatus?.recentLogs &&
                containerPrepareStatus.recentLogs.length > 0 && (
                  <div className="mt-3 max-h-40 overflow-auto rounded-lg border border-border/50 bg-background/50 px-3 py-2 text-left font-mono text-[11px] text-muted-foreground">
                    {containerPrepareStatus.recentLogs.map(
                      (line: string, index: number) => (
                        <div key={`${index}-${line}`} className="truncate">
                          {line}
                        </div>
                      ),
                    )}
                  </div>
                )}
              {isContainerPrepareFailed && !profileSessionsLoading && (
                <div className="mt-4 flex items-center justify-center gap-2">
                  {requiresManagedPodmanApproval && (
                    <Button
                      onClick={() => {
                        const allowed = window.confirm(
                          "Allow May to download and install managed Podman for container profiles?",
                        );
                        if (!allowed) {
                          return;
                        }
                        void refreshProfileSessions({
                          allowManagedPodmanDownload: true,
                        });
                      }}
                    >
                      Allow Podman Download
                    </Button>
                  )}
                  <Button
                    variant="secondary"
                    onClick={() => void refreshProfileSessions()}
                  >
                    Retry
                  </Button>
                </div>
              )}
            </div>
          </div>
        ) : chatSurfaceState === "profile-loading" ? (
          <div className="flex-1 min-h-0 px-4 pt-10">
            <div className="mx-auto w-full max-w-3xl space-y-4">
              <div className="h-4 w-40 rounded-full bg-muted/70 animate-pulse" />
              <div className="space-y-3 rounded-2xl border border-border/50 bg-background/70 p-4">
                <div className="h-3 w-3/4 rounded-full bg-muted/60 animate-pulse" />
                <div className="h-3 w-5/6 rounded-full bg-muted/50 animate-pulse [animation-delay:120ms]" />
                <div className="h-3 w-2/3 rounded-full bg-muted/40 animate-pulse [animation-delay:240ms]" />
              </div>
              <div className="space-y-3 rounded-2xl border border-border/40 bg-background/60 p-4">
                <div className="h-3 w-2/3 rounded-full bg-muted/60 animate-pulse [animation-delay:160ms]" />
                <div className="h-3 w-3/4 rounded-full bg-muted/50 animate-pulse [animation-delay:280ms]" />
              </div>
            </div>
          </div>
        ) : chatSurfaceState === "welcome" ? (
          <WelcomeScreen />
        ) : (
          <ChatMessageList
            messages={messages}
            currentSessionId={currentSessionId}
            scrollContainerRef={scrollContainerRef}
            contentRef={contentRef}
            onScroll={handleScroll}
            onOpenPreview={handleOpenPreview}
            onRetry={(request) => void handleSend(request)}
          />
        )}

        {todoProgress &&
          todoProgress.items.length > 0 &&
          hasPendingAssistantMessage && (
            <TodoProgressBar progress={todoProgress} />
          )}

        {showComposer && (
          <div className="shrink-0 px-4 pb-2 pt-2 bg-transparent w-full z-20 pointer-events-none">
            <div className="max-w-3xl mx-auto w-full pointer-events-auto">
              {pendingPermission && (
                <PermissionPrompt
                  request={pendingPermission}
                  isSubmitting={isPermissionSubmitting}
                  onDecide={(approved) => void handlePermissionDecision(approved)}
                />
              )}
              {pendingWorkspace && (
                <WorkspacePrompt
                  request={pendingWorkspace}
                  isSubmitting={isWorkspaceSubmitting}
                  onDecide={(approved) => void handleWorkspaceDecision(approved)}
                />
              )}
              {pendingAskUser && (
                <AskUserPrompt
                  state={pendingAskUser}
                  isSubmitting={isAskUserSubmitting}
                  onAnswerChange={handleAskUserAnswerChange}
                  onSubmit={() => void handleAskUserSubmit()}
                  onSkipAll={() => {
                    const skipped = pendingAskUser.request.questions.map(
                      (q) => ({
                        section: q.section,
                        answer: { kind: "skipped" as const },
                      }),
                    );
                    void handleAskUserSubmit(skipped);
                  }}
                />
              )}
              {uiError && (
                <div className="mb-3 rounded-lg border border-destructive bg-destructive px-3 py-2 text-sm text-destructive-foreground">
                  {uiError}
                </div>
              )}
              <ActivityRail
                pendingMessages={queuedMessages}
                runningCommands={runningTerminalCommands}
                backgroundCommand={backgroundTerminalTask}
                sessionId={currentSessionId}
                onSteerMessage={steerQueuedMessage}
                onRemoveMessage={removeQueuedMessage}
              />
              <Composer
                inputValue={inputValue}
                onInputChange={setInputValue}
                placeholder={composerPlaceholder}
                attachments={attachments}
                onRemoveAttachment={handleRemoveAttachment}
                onPaste={handlePaste}
                onFileSelect={handleFileSelect}
                fileInputRef={fileInputRef}
                composerInputRef={composerInputRef}
                isNewChat={isNewChat}
                canSend={canSend}
                onSend={() => void handleSend()}
                onScreenshot={handleScreenshot}
                currentModelId={currentModelId}
                onModelChange={handleModelChange}
                isModelUpdating={isModelUpdating}
                isSending={isSending}
                currentModelSupportsReasoning={currentModelSupportsReasoning}
                currentReasoningEffort={currentReasoningEffort}
                onReasoningEffortChange={handleReasoningEffortChange}
                showTokenUsage={showTokenUsage}
                currentSessionId={currentSessionId}
                promptTokens={latestStats?.promptTokens}
                contextUsedTokens={contextUsedTokens}
                contextWindow={contextWindow}
                contextUsagePercent={contextUsagePercent}
                contextUsageTitle={contextUsageTitle}
                showStopButton={showStopButton}
                showSendButton={showComposerSendButton}
                onStop={handleStop}
              />
            </div>
          </div>
        )}
      </div>
      <AnimatePresence initial={false} mode="wait">
        {activeRightPanel === "artifacts" && (
          <>
            <div
              role="separator"
              aria-orientation="vertical"
              aria-label="Resize artifacts panel"
              className={`group relative h-full w-2 flex-shrink-0 touch-none cursor-col-resize ${
                isArtifactPanelResizing ? "bg-primary/10" : ""
              }`}
              onPointerDown={handleArtifactResizerPointerDown}
            >
              <div
                className={`absolute inset-y-0 left-1/2 w-px -translate-x-1/2 ${
                  isArtifactPanelResizing
                    ? "bg-primary"
                    : "bg-border/80 group-hover:bg-primary/70"
                }`}
              />
            </div>
            <motion.aside
              key="artifacts-panel"
              initial={{ width: 0, opacity: 0 }}
              animate={{ width: artifactPanelWidth, opacity: 1 }}
              exit={{ width: 0, opacity: 0 }}
              transition={{ duration: 0.24, ease: [0.22, 1, 0.36, 1] }}
              className="h-full flex-shrink-0 overflow-hidden"
            >
              <ArtifactsSidebar
                open
                artifacts={artifacts}
                selectedPath={selectedPreviewPath}
                preview={previewPane}
                showHistory={showArtifactHistory}
                onToggleHistory={toggleArtifactHistory}
                onClose={() => setIsArtifactsOpen(false)}
                getViewerForPath={getArtifactViewerHint}
                onSelect={(target) => {
                  void openPreview(target);
                }}
              />
            </motion.aside>
          </>
        )}

        {activeRightPanel === "browser" && (
          <motion.aside
            key="browser-panel"
            initial={{ width: 0, opacity: 0 }}
            animate={{ width: 720, opacity: 1 }}
            exit={{ width: 0, opacity: 0 }}
            transition={{ duration: 0.24, ease: [0.22, 1, 0.36, 1] }}
            className="h-full flex-shrink-0 overflow-hidden"
          >
            <BrowserView
              open
              onClose={() => setIsBrowserOpen(false)}
              sessionId={currentSessionId}
            />
          </motion.aside>
        )}
      </AnimatePresence>
    </div>
  );
}
