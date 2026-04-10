import { useEffect, useRef, useState, useCallback, useMemo } from "react";
import { useStore } from "@/lib/store";
import {
  setSessionPlanState as apiSetSessionPlanState,
  type AskUserRequest,
  type PermissionRequest,
  type WorkspaceRequest,
} from "@/lib/api";
import { RightPanel } from "./RightPanel";
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
import { BackendDownScreen } from "./status/BackendDownScreen";
import { ContainerPreparingScreen } from "./status/ContainerPreparingScreen";
import { ProfileLoadingScreen } from "./status/ProfileLoadingScreen";
import {
  buildAttachmentFromFile,
  buildAttachmentFromPastedFile,
  buildAttachmentFromPastedText,
} from "@/lib/attachments";
import { useApprovalHandlers } from "@/hooks/useApprovalHandlers";
import { useArtifactPanel } from "@/hooks/useArtifactPanel";
import { useArtifactPanelResize } from "@/hooks/useArtifactPanelResize";
import { useChatAutoScroll } from "@/hooks/useChatAutoScroll";
import { useQueuedMessageActions } from "@/hooks/useQueuedMessageActions";
import { useSettleOrphanedTimelines } from "@/hooks/useSettleOrphanedTimelines";
import { useStreamController } from "@/hooks/useStreamController";
import { LONG_TEXT_THRESHOLD } from "./chatUtils";

import type {
  ArtifactPreviewTarget,
  Attachment,
  TerminalExecution,
  SessionPlanState,
} from "@/types";
import { useRuntimeStore } from "@/lib/runtimeStore";
import {
  isManagedPodmanApprovalCode,
  resolveWorkspaceSurfaceState,
} from "@/lib/containerPrepare";
import { getModel } from "@/lib/constants";

interface ChatInterfaceProps {
  onToggleSidebar: () => void;
  isSidebarOpen: boolean;
}

export function ChatInterface({
  onToggleSidebar,
  isSidebarOpen,
}: ChatInterfaceProps) {
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
  const [pendingAskUser, setPendingAskUser] = useState<AskUserState | null>(
    null,
  );
  const [isBrowserOpen, setIsBrowserOpen] = useState(false);
  const [todoProgress, setTodoProgress] = useState<TodoProgressState | null>(null);
  const composerInputRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);
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

  const {
    scrollContainerRef,
    contentRef,
    userScrolledUpRef,
    onScroll: handleScroll,
  } = useChatAutoScroll({
    currentSessionId,
    isChatSurfaceActive: chatSurfaceState === "chat",
    messages,
  });
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

  useSettleOrphanedTimelines({
    currentSessionId,
    messages,
    isStreamActive: isGenerating || isSending || isStreamControllerActive,
    replaceSessionMessages,
  });

  const { removeQueuedMessage, steerQueuedMessage } = useQueuedMessageActions({
    currentSessionId,
    queuedMessages,
    setInputValue,
    setAttachments,
    setError: setUiError,
    composerInputRef,
  });

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

  const { handleSend, handleStop } = useStreamController({
    currentSessionId,
    newChatModelId,
    configuredModelIds,
    inputValue,
    attachments,
    setInputValue,
    setAttachments,
    setUiError,
    setIsSending,
    setIsStreamControllerActive,
    setIsBrowserOpen,
    setTodoProgress,
    userScrolledUpRef,
    createSession,
    addMessage,
    replaceSessionMessages,
  });


  // legacy handleStop removed — using hook version from useStreamController

  const {
    isPermissionSubmitting,
    isWorkspaceSubmitting,
    isAskUserSubmitting,
    onPermissionDecision: handlePermissionDecision,
    onWorkspaceDecision: handleWorkspaceDecision,
    onAskUserAnswerChange: handleAskUserAnswerChange,
    onAskUserSubmit: handleAskUserSubmit,
  } = useApprovalHandlers({
    currentSessionId,
    pendingPermission,
    pendingWorkspace,
    pendingAskUser,
    setPendingAskUser,
    setError: setUiError,
  });

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

  const {
    width: artifactPanelWidth,
    isResizing: isArtifactPanelResizing,
    onResizerPointerDown: handleArtifactResizerPointerDown,
  } = useArtifactPanelResize({ isSidebarOpen });

  if (chatSurfaceState === "backend-down") {
    return (
      <BackendDownScreen
        reason={backendUnavailableReason}
        onRetry={() => void refreshProfileSessions()}
      />
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
          <ContainerPreparingScreen
            isPrepareFailed={isContainerPrepareFailed}
            summary={containerPrepareSummary}
            recentLogs={containerPrepareStatus?.recentLogs ?? []}
            profileSessionsLoading={profileSessionsLoading}
            requiresManagedPodmanApproval={requiresManagedPodmanApproval}
            onRetry={() => void refreshProfileSessions()}
            onApproveManagedPodmanDownload={() => {
              const allowed = window.confirm(
                "Allow May to download and install managed Podman for container profiles?",
              );
              if (!allowed) return;
              void refreshProfileSessions({ allowManagedPodmanDownload: true });
            }}
          />
        ) : chatSurfaceState === "profile-loading" ? (
          <ProfileLoadingScreen />
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
      <RightPanel
        active={activeRightPanel}
        artifacts={artifacts}
        selectedPreviewPath={selectedPreviewPath}
        previewPane={previewPane}
        showArtifactHistory={showArtifactHistory}
        onToggleArtifactHistory={toggleArtifactHistory}
        onCloseArtifacts={() => setIsArtifactsOpen(false)}
        getViewerForPath={getArtifactViewerHint}
        onSelectArtifact={(target) => void openPreview(target)}
        artifactPanelWidth={artifactPanelWidth}
        isArtifactPanelResizing={isArtifactPanelResizing}
        onArtifactResizerPointerDown={handleArtifactResizerPointerDown}
        currentSessionId={currentSessionId}
        onCloseBrowser={() => setIsBrowserOpen(false)}
      />
    </div>
  );
}
