import { useRef, useEffect, useState, useCallback, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import { AnimatePresence, motion } from "framer-motion";
import { useStore } from "@/lib/store";
import {
  fetchArtifactPreview,
  mapContainerLocalArtifactUrl,
  prepareSlidevPreview,
  fetchSessionArtifacts,
  fetchSessionRuntime,
  isSessionActiveConflictError,
  fetchSessionTasks,
  enqueueChat,
  removePendingSessionMessage,
  respondAskUserRequest,
  respondPermissionRequest,
  respondWorkspaceRequest,
  saveSessionTasks,
  stopSessionRun,
  streamChat,
  subscribeSessionArtifacts,
  type AskUserAnswer,
  type AskUserRequest,
  type PermissionRequest,
  type WorkspaceRequest,
} from "@/lib/api";
import { MessageItem } from "./MessageItem";
import { ModelSelector } from "./ModelSelector";
import { AttachmentPreview } from "./AttachmentPreview";
import { AttachmentCard } from "./AttachmentCard";
import { ArtifactsSidebar } from "./ArtifactsSidebar";
import { BrowserView } from "./BrowserView";
import { ContextInspector } from "./ContextInspectorDialog";
import { PendingFollowUpPanel } from "./PendingFollowUpPanel";
import {
  createDisconnectBanner,
  collectRetryBatch,
  buildArtifactViewerHints,
  parseToolResultPayload,
  settleTimelineSegments,
  summarizeTimelineSegments,
  ARTIFACT_PANEL_MIN_WIDTH,
  ARTIFACT_PANEL_DEFAULT_WIDTH,
  ARTIFACT_PANEL_MIN_CHAT_WIDTH,
  ARTIFACT_PANEL_MAX_VIEWPORT_RATIO,
  LONG_TEXT_THRESHOLD,
  type QueuedMessage,
  type PreviewPaneState,
} from "./chatUtils";

import { Textarea } from "@/components/ui/textarea";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Send,
  Plus,
  X,
  File,
  FileText,
  Pencil,
  Image as ImageIcon,
  Monitor,
  PanelLeft,
  Share2,
  Square,
  CircleX,
} from "lucide-react";
import type {
  Artifact,
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

interface AskUserState {
  request: AskUserRequest;
  answers: AskUserAnswer[];
}

interface ChatInterfaceProps {
  onToggleSidebar: () => void;
  isSidebarOpen: boolean;
}

interface TerminalToolStartArgs {
  description: string;
  script: string;
  mode?: TerminalExecution["mode"];
}

function normalizeToolName(rawName: unknown): string {
  if (typeof rawName !== "string") {
    return "";
  }
  return rawName.trim().toLowerCase();
}

function isWebSearchToolName(normalizedName: string): boolean {
  return (
    normalizedName === "websearch" ||
    normalizedName.endsWith(".websearch") ||
    normalizedName.endsWith("/websearch")
  );
}

function isWebFetchToolName(normalizedName: string): boolean {
  return (
    normalizedName === "webfetch" ||
    normalizedName.endsWith(".webfetch") ||
    normalizedName.endsWith("/webfetch")
  );
}

function isTerminalExecutionMode(value: unknown): value is TerminalExecution["mode"] {
  return (
    value === "default" ||
    value === "sandboxed" ||
    value === "unsafe" ||
    value === "ssh"
  );
}

function parseTerminalToolStartArgs(raw: unknown): TerminalToolStartArgs {
  const parsed =
    typeof raw === "string"
      ? JSON.parse(raw)
      : raw;
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("terminal tool payload must be an object");
  }
  const record = parsed as Record<string, unknown>;
  const description =
    typeof record.description === "string" ? record.description.trim() : "";
  if (!description) {
    throw new Error("terminal tool payload is missing a non-empty description");
  }
  const script =
    typeof record.script === "string"
      ? record.script
      : typeof record.command === "string"
        ? record.command
        : "";
  if (!script) {
    throw new Error("terminal tool payload is missing script");
  }
  const mode = isTerminalExecutionMode(record.mode) ? record.mode : undefined;
  return { description, script, mode };
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
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [isArtifactsOpen, setIsArtifactsOpen] = useState(false);
  const [isBrowserOpen, setIsBrowserOpen] = useState(false);
  const [showArtifactHistory, setShowArtifactHistory] = useState(false);
  const [selectedPreviewPath, setSelectedPreviewPath] = useState<string | null>(
    null,
  );
  const [selectedPreviewViewer, setSelectedPreviewViewer] =
    useState<ArtifactPreviewTarget["viewer"]>(undefined);
  const [previewPane, setPreviewPane] = useState<PreviewPaneState | null>(null);
  const [artifactPanelWidth, setArtifactPanelWidth] = useState(
    ARTIFACT_PANEL_DEFAULT_WIDTH,
  );
  const [isArtifactPanelResizing, setIsArtifactPanelResizing] = useState(false);
  const [todoProgress, setTodoProgress] = useState<{
    items: Array<{ text: string; checked: boolean }>;
    total: number;
    completed: number;
  } | null>(null);
  const previewBlobUrlRef = useRef<string | null>(null);
  const selectedPreviewPathRef = useRef<string | null>(null);
  const selectedPreviewViewerRef =
    useRef<ArtifactPreviewTarget["viewer"]>(undefined);
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
  const previewSessionRef = useRef<string | null>(null);
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

  const handlePlanStateChange = useCallback(
    async (nextPlanState: SessionPlanState) => {
      if (!currentSessionId) {
        return;
      }
      try {
        const payload = await fetchSessionTasks(currentSessionId);
        const saved = await saveSessionTasks(currentSessionId, {
          content: payload.content,
          planState: nextPlanState,
        });
        setSessionPlanState(currentSessionId, saved.planState);
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
  const hasInput = inputValue.trim() || attachments.length > 0;
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
  const showStopButton =
    isGenerating ||
    isSending ||
    isStreamControllerActive ||
    hasPendingAssistantMessage;
  const pendingPanelTitle =
    queuedMessages.length > 1 ? "Pending follow-ups" : "Pending follow-up";
  const pendingPanelSubtitle = runtimeSession?.state === "running"
    ? "Will send after the active run settles"
    : runtimeSession?.pendingImmediate
      ? "Waiting to dispatch next"
      : "Saved for later in this session";
  const markRuntimeRunning = useRuntimeStore((state) => state.markRunning);
  const applyRuntime = useRuntimeStore((state) => state.applyRuntime);
  const markRuntimeFirstToken = useRuntimeStore(
    (state) => state.markFirstToken,
  );

  const normalizePreviewUrl = useCallback((value: string): string | null => {
    const trimmed = value.trim();
    if (!trimmed) {
      return null;
    }

    try {
      const parsed = new URL(trimmed);
      if (parsed.protocol === "http:" || parsed.protocol === "https:") {
        return parsed.toString();
      }
      return null;
    } catch {
      const localhostPattern =
        /^(localhost|127(?:\.\d{1,3}){3}|0\.0\.0\.0|\[::1\]|::1|host\.docker\.internal|host\.containers\.internal)(:\d+)?(\/.*)?$/i;
      if (!localhostPattern.test(trimmed)) {
        return null;
      }
      try {
        return new URL(`http://${trimmed}`).toString();
      } catch {
        return null;
      }
    }
  }, []);

  const normalizeArtifactPath = useCallback((value: string): string => {
    const trimmed = value.trim();
    if (!trimmed) {
      return "";
    }

    const slashNormalized = trimmed.replace(/\\/g, "/");
    const withoutDotPrefix = slashNormalized.replace(/^\.\/+/, "");
    const lower = withoutDotPrefix.toLowerCase();
    if (lower === "artifacts") {
      return "";
    }
    if (lower.startsWith("artifacts/")) {
      return withoutDotPrefix.slice("artifacts/".length);
    }
    return withoutDotPrefix;
  }, []);

  const isPreviewUrl = useCallback(
    (value: string): boolean => {
      return normalizePreviewUrl(value) !== null;
    },
    [normalizePreviewUrl],
  );

  const artifactViewerHints = useMemo(
    () => buildArtifactViewerHints(messages, normalizeArtifactPath),
    [messages, normalizeArtifactPath],
  );

  const getArtifactViewerHint = useCallback(
    (rawPath: string): ArtifactPreviewTarget["viewer"] => {
      const normalized = normalizeArtifactPath(rawPath);
      if (!normalized) {
        return undefined;
      }
      return artifactViewerHints.get(normalized);
    },
    [artifactViewerHints, normalizeArtifactPath],
  );

  const pickPreferredArtifactTarget = useCallback(
    (nextArtifacts: Artifact[]): ArtifactPreviewTarget | null => {
      if (nextArtifacts.length === 0) {
        return null;
      }
      const artifactPaths = new Set(
        nextArtifacts.map((artifact) => artifact.path),
      );
      const hintedEntries = Array.from(artifactViewerHints.entries());
      for (let index = hintedEntries.length - 1; index >= 0; index -= 1) {
        const [path, viewer] = hintedEntries[index];
        if (artifactPaths.has(path)) {
          return { path, viewer };
        }
      }
      const fallbackPath = nextArtifacts[0].path;
      return {
        path: fallbackPath,
        viewer: getArtifactViewerHint(fallbackPath),
      };
    },
    [artifactViewerHints, getArtifactViewerHint],
  );

  const refreshArtifacts = useCallback(async (): Promise<Artifact[]> => {
    if (!currentSessionId) {
      setArtifacts([]);
      return [];
    }
    try {
      const next = await fetchSessionArtifacts(currentSessionId);
      setArtifacts(next);
      if (next.length === 0) {
        // Keep preview pane open to avoid flicker while artifacts index catches up.
        return next;
      }

      const currentPreviewPath = selectedPreviewPathRef.current;
      if (currentPreviewPath && !isPreviewUrl(currentPreviewPath)) {
        const selectedArtifactPath = normalizeArtifactPath(currentPreviewPath);
        const exists = next.some((a) => a.path === selectedArtifactPath);
        if (!exists) {
          const fallbackTarget = pickPreferredArtifactTarget(next);
          if (!fallbackTarget) {
            return next;
          }
          setSelectedPreviewPath(fallbackTarget.path);
          setSelectedPreviewViewer(fallbackTarget.viewer);
          selectedPreviewPathRef.current = fallbackTarget.path;
          selectedPreviewViewerRef.current = fallbackTarget.viewer;
          setIsArtifactsOpen(true);
        } else {
          const hintedViewer = getArtifactViewerHint(selectedArtifactPath);
          if (selectedPreviewViewerRef.current !== hintedViewer) {
            setSelectedPreviewViewer(hintedViewer);
            selectedPreviewViewerRef.current = hintedViewer;
          }
        }
      } else if (!currentPreviewPath) {
        const fallbackTarget = pickPreferredArtifactTarget(next);
        if (fallbackTarget) {
          setSelectedPreviewPath(fallbackTarget.path);
          setSelectedPreviewViewer(fallbackTarget.viewer);
          selectedPreviewPathRef.current = fallbackTarget.path;
          selectedPreviewViewerRef.current = fallbackTarget.viewer;
          setIsArtifactsOpen(true);
        }
      }
      return next;
    } catch (error) {
      setUiError(
        `Failed to load artifacts: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
      return [];
    }
  }, [
    currentSessionId,
    getArtifactViewerHint,
    isPreviewUrl,
    normalizeArtifactPath,
    pickPreferredArtifactTarget,
  ]);

  const openPreview = useCallback(
    async (target: ArtifactPreviewTarget | string) => {
      const request = typeof target === "string" ? { path: target } : target;
      const rawPath = request.path.trim();
      if (!rawPath || !currentSessionId) return;

      const normalizedUrl = normalizePreviewUrl(rawPath);
      const previewPath = normalizedUrl ?? normalizeArtifactPath(rawPath);
      if (!previewPath) {
        setUiError("artifact path is required");
        setPreviewPane({
          kind: "text",
          text: "Preview failed: artifact path is required",
        });
        return;
      }

      setSelectedPreviewPath(previewPath);
      setSelectedPreviewViewer(request.viewer);
      selectedPreviewPathRef.current = previewPath;
      selectedPreviewViewerRef.current = request.viewer;
      setIsArtifactsOpen(true);
      setShowArtifactHistory(false);

      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }

      if (normalizedUrl) {
        // For container profiles, delegate host detection to the backend by
        // calling the local-url-map endpoint.  The backend is the single source of
        // truth for what constitutes a mappable local host (eliminating the previous
        // duplicated `isLocalPreviewHost` check on the frontend).
        let resolvedUrl = normalizedUrl;
        const isContainerProfile =
          currentProfile?.environment.type === "container";
        if (isContainerProfile && currentSessionId) {
          try {
            resolvedUrl = await mapContainerLocalArtifactUrl(
              currentSessionId,
              normalizedUrl,
            );
          } catch {
            // Backend rejected the URL (not a local host, or missing port, etc.) —
            // fall through to show the original URL directly.
          }
        }

        if (request.viewer === "slidev") {
          setPreviewPane({
            kind: "slidev",
            url: resolvedUrl,
          });
          return;
        }

        setPreviewPane({
          kind: "external",
          url: resolvedUrl,
        });
        return;
      }

      try {
        if (request.viewer === "slidev") {
          const previewUrl = await prepareSlidevPreview(
            currentSessionId,
            previewPath,
          );
          setPreviewPane({ kind: "slidev", url: previewUrl });
          return;
        }

        const response = await fetchArtifactPreview(
          currentSessionId,
          previewPath,
        );
        const contentType = (
          response.headers.get("content-type") || ""
        ).toLowerCase();
        const ext = previewPath.split(".").pop()?.toLowerCase() || "";
        const arrayBuffer = await response.arrayBuffer();
        const bytes = new Uint8Array(arrayBuffer);

        const text =
          contentType.startsWith("text/") ||
          contentType.includes("json") ||
          contentType.includes("xml") ||
          ["md", "markdown", "txt", "log", "csv"].includes(ext)
            ? new TextDecoder().decode(bytes)
            : "";

        const isMarkdown =
          contentType.includes("markdown") || ["md", "markdown"].includes(ext);
        if (isMarkdown) {
          setPreviewPane({ kind: "markdown", markdown: text });
        } else if (
          contentType.startsWith("text/plain") ||
          ["txt", "log", "csv", "json", "xml"].includes(ext)
        ) {
          setPreviewPane({ kind: "text", text });
        } else if (
          ext === "docx" ||
          contentType.includes("officedocument.wordprocessingml.document")
        ) {
          const mammoth = await import("mammoth");
          const htmlResult = await mammoth.default.convertToHtml({
            arrayBuffer,
          });
          const htmlBlob = new Blob([htmlResult.value], {
            type: "text/html;charset=utf-8",
          });
          const htmlUrl = URL.createObjectURL(htmlBlob);
          previewBlobUrlRef.current = htmlUrl;
          setPreviewPane({ kind: "iframe", url: htmlUrl });
        } else {
          const blob = new Blob([bytes], {
            type:
              response.headers.get("content-type") ||
              "application/octet-stream",
          });
          const objectUrl = URL.createObjectURL(blob);
          previewBlobUrlRef.current = objectUrl;

          if (contentType.startsWith("image/")) {
            setPreviewPane({ kind: "image", url: objectUrl });
          } else if (contentType.startsWith("audio/")) {
            setPreviewPane({ kind: "audio", url: objectUrl });
          } else {
            setPreviewPane({ kind: "iframe", url: objectUrl });
          }
        }
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        setUiError(`Artifact preview failed: ${reason}`);
        setPreviewPane({ kind: "text", text: `Preview failed: ${reason}` });
      }
    },
    [
      currentProfile?.environment.type,
      currentSessionId,
      normalizeArtifactPath,
      normalizePreviewUrl,
    ],
  );

  const handleOpenPreview = useCallback(
    (target: ArtifactPreviewTarget) => {
      void openPreview(target);
    },
    [openPreview],
  );

  useEffect(() => {
    selectedPreviewPathRef.current = selectedPreviewPath;
  }, [selectedPreviewPath]);

  useEffect(() => {
    selectedPreviewViewerRef.current = selectedPreviewViewer;
  }, [selectedPreviewViewer]);

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
    const nextPreviewSessionId = currentSessionId ?? null;
    const isSessionChanged = previewSessionRef.current !== nextPreviewSessionId;
    if (isSessionChanged) {
      previewSessionRef.current = nextPreviewSessionId;
      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }
      setArtifacts([]);
      setShowArtifactHistory(false);
      setSelectedPreviewPath(null);
      setSelectedPreviewViewer(undefined);
      selectedPreviewPathRef.current = null;
      selectedPreviewViewerRef.current = undefined;
      setPreviewPane(null);
      setIsArtifactsOpen(false);
    }

    if (!currentSessionId) {
      return;
    }

    void refreshArtifacts();
    const banner = createDisconnectBanner(
      setUiError,
      "Artifacts stream disconnected; reconnecting...",
    );

    const unsubscribe = subscribeSessionArtifacts(currentSessionId, {
      onArtifacts: () => {
        banner.clear();
        void (async () => {
          const nextArtifacts = await refreshArtifacts();
          const currentPreviewPath = selectedPreviewPathRef.current;
          if (nextArtifacts.length === 0 || !currentPreviewPath) {
            return;
          }
          if (isPreviewUrl(currentPreviewPath)) {
            return;
          }
          const stillExists = nextArtifacts.some(
            (artifact) => artifact.path === currentPreviewPath,
          );
          if (!stillExists) {
            return;
          }
          await openPreview({
            path: currentPreviewPath,
            viewer: selectedPreviewViewerRef.current,
          });
        })();
      },
      onOpen: () => banner.clear(),
      onHeartbeat: () => banner.clear(),
      onError: () => banner.schedule(),
    });

    return () => {
      banner.cleanup();
      unsubscribe();
    };
  }, [currentSessionId, isPreviewUrl, openPreview, refreshArtifacts]);

  useEffect(() => {
    if (!currentSessionId || !selectedPreviewPath || previewPane !== null) {
      return;
    }
    if (!isPreviewUrl(selectedPreviewPath)) {
      const exists = artifacts.some(
        (artifact) => artifact.path === selectedPreviewPath,
      );
      if (!exists) {
        return;
      }
    }
    void openPreview({
      path: selectedPreviewPath,
      viewer: selectedPreviewViewer,
    });
  }, [
    artifacts,
    currentSessionId,
    isPreviewUrl,
    openPreview,
    previewPane,
    selectedPreviewPath,
    selectedPreviewViewer,
  ]);

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
      const candidateModelId = newChatModelId.trim();
      if (!candidateModelId) {
        setUiError(
          "No model configured. Please select a model before sending.",
        );
        return;
      }
      if (
        configuredModelIds.size > 0 &&
        !configuredModelIds.has(candidateModelId)
      ) {
        setUiError("Selected model is not available in the current profile.");
        return;
      }
    } else {
      const targetSession = useStore
        .getState()
        .sessions.find((session) => session.id === requestedSessionId);
      const targetModelId = targetSession?.model?.trim() ?? "";
      if (!targetModelId) {
        setUiError(
          "No model configured for this session. Please select a model.",
        );
        return;
      }
      if (
        configuredModelIds.size > 0 &&
        !configuredModelIds.has(targetModelId)
      ) {
        setUiError(
          "Session model is no longer available. Please choose another model.",
        );
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
    const targetGenerating =
      !!targetRuntime &&
      (targetRuntime.state !== "idle" ||
        targetRuntime.pendingImmediate != null ||
        (targetRuntime.queuedMessages?.length ?? 0) > 0);
    const deliveryMode = options?.deliveryMode ?? "steer";

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

    const cloneTimelineSegments = (
      segments: TimelineSegment[],
    ): TimelineSegment[] =>
      segments.map((segment) =>
        segment.kind === "terminal"
          ? {
              ...segment,
              commands: segment.commands.map((command) => ({ ...command })),
            }
          : segment.kind === "tool"
            ? {
                ...segment,
                tool: { ...segment.tool },
              }
            : { ...segment },
      );

    const collectTerminalExecutions = (
      segments: TimelineSegment[],
    ): TerminalExecution[] =>
      segments.flatMap((segment) =>
        segment.kind === "terminal"
          ? segment.commands.map((command) => ({ ...command }))
          : [],
      );

    const collectThinkingText = (segments: TimelineSegment[]): string =>
      segments
        .filter(
          (
            segment,
          ): segment is Extract<TimelineSegment, { kind: "thinking" }> =>
            segment.kind === "thinking",
        )
        .map((segment) => segment.content)
        .join("\n\n")
        .trim();

    const isAnyThinkingRunning = (segments: TimelineSegment[]): boolean =>
      segments.some(
        (segment) => segment.kind === "thinking" && segment.isRunning,
      );

    const closeActiveThinkingSegment = () => {
      if (!activeThinkingSegmentId) {
        return;
      }
      const endedAt = Date.now();
      timelineSegments = timelineSegments.map((segment) =>
        segment.kind === "thinking" && segment.id === activeThinkingSegmentId
          ? {
              ...segment,
              isRunning: false,
              endedAt: segment.endedAt ?? endedAt,
            }
          : segment,
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
          const { output, failed } = parseToolResultPayload(tool.result);
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
                      status: (failed
                        ? "failed"
                        : "done") as TerminalExecution["status"],
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
                    status: failed ? "failed" : "done",
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
        const terminalError = !completedNormally && !wasStoppedByUser()
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

      void syncSessionRuntime(sessionId).catch((error) => {
        setUiError(
          `Failed to sync runtime state: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      });
    }
  };

  const handleStop = () => {
    const runtimeSessionId = streamingSessionIdRef.current ?? currentSessionId;
    if (!runtimeSessionId) {
      return;
    }
    stopRequestedSessionIdsRef.current.add(runtimeSessionId);
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    setIsStreamControllerActive(false);
    setIsSending(false);
    setIsBrowserOpen(false);
    const waitForStop = waitForStoppedSession(runtimeSessionId).finally(() => {
      stopWaitersRef.current.delete(runtimeSessionId);
      stopRequestedSessionIdsRef.current.delete(runtimeSessionId);
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

  const permissionToneClass = pendingPermission
    ? pendingPermission.mode === "unsafe"
      ? "border-red-300 bg-red-100 text-red-900 dark:border-red-800 dark:bg-red-950/95 dark:text-red-100"
      : "border-border bg-card text-foreground"
    : "";
  const workspaceToneClass = pendingWorkspace
    ? "border-border bg-card text-foreground"
    : "";

  const handleTitleSubmit = () => {
    if (currentSessionId && titleInput.trim())
      updateSessionTitle(currentSessionId, titleInput.trim());
    setIsEditingTitle(false);
  };

  const handleScreenshot = async () => {
    try {
      const stream = await navigator.mediaDevices.getDisplayMedia({
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
      stream.getTracks().forEach((t) => t.stop());
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
    }
  };

  const classifyFile = (file: File): Attachment["type"] => {
    if (file.type.startsWith("image/")) return "image";
    if (file.type === "application/pdf") return "pdf";
    if (
      file.type ===
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" ||
      file.name.endsWith(".docx")
    )
      return "document";
    if (
      file.type === "text/plain" ||
      file.name.endsWith(".txt") ||
      file.name.endsWith(".md")
    )
      return "document";
    return "file";
  };

  const readTextFile = async (file: File): Promise<string> => {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () =>
        reject(
          new Error(
            `failed to read text file ${file.name}: ${
              reader.error?.message ?? "unknown error"
            }`,
          ),
        );
      reader.readAsText(file);
    });
  };

  const readFileAsDataUrl = async (file: File): Promise<string> => {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () =>
        resolve(typeof reader.result === "string" ? reader.result : "");
      reader.onerror = () =>
        reject(
          new Error(
            `failed to read file ${file.name}: ${
              reader.error?.message ?? "unknown error"
            }`,
          ),
        );
      reader.readAsDataURL(file);
    });
  };

  const processDocxFile = async (file: File): Promise<string> => {
    try {
      const mammoth = await import("mammoth");
      const arrayBuffer = await file.arrayBuffer();
      const result = await mammoth.default.extractRawText({ arrayBuffer });
      return result.value;
    } catch (error) {
      throw new Error(
        `failed to parse DOCX ${file.name}: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
  };

  const handleFileSelect = async (e: React.ChangeEvent<HTMLInputElement>) => {
    if (!e.target.files?.length) return;
    try {
      const files = Array.from(e.target.files!);
      const newAttachments: Attachment[] = [];

      for (const f of files) {
        const fileType = classifyFile(f);
        const att: Attachment = {
          id: crypto.randomUUID(),
          name: f.name,
          type: fileType,
          url: URL.createObjectURL(f),
          uploadDataUrl: await readFileAsDataUrl(f),
        };

        if (fileType === "document") {
          if (f.name.endsWith(".docx")) {
            const text = await processDocxFile(f);
            att.textContent = text;
            att.textPreview = text.slice(0, 200);
          } else {
            const text = await readTextFile(f);
            att.textContent = text;
            att.textPreview = text.slice(0, 200);
          }
        }

        newAttachments.push(att);
      }

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

    const newAttachments: Attachment[] = [];

    for (const item of items) {
      if (item.kind !== "file") continue;
      const file = item.getAsFile();
      if (!file) continue;

      const fileType = classifyFile(file);
      const att: Attachment = {
        id: crypto.randomUUID(),
        name:
          file.name || (fileType === "image" ? "Pasted Image" : "Pasted File"),
        type: fileType,
        url: URL.createObjectURL(file),
        uploadDataUrl: await readFileAsDataUrl(file),
      };
      newAttachments.push(att);
    }

    if (shouldAttachLargeText) {
      const preview = text.slice(0, 200);
      newAttachments.push({
        id: crypto.randomUUID(),
        name: "Pasted Text",
        type: "document",
        url: "",
        textContent: text,
        textPreview: preview,
      });
    }

    if (newAttachments.length > 0) {
      setAttachments((prev) => [...prev, ...newAttachments]);
    }
  };

  const currentModelId = isNewChat ? newChatModelId : currentSession.model;
  const currentReasoningEffort = isNewChat
    ? newChatReasoningEffort
    : currentSession.reasoningEffort;

  const canShowArtifactsPanel = artifacts.length > 0 || !!selectedPreviewPath;
  const activeRightPanel = isBrowserOpen
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

  useEffect(() => {
    return () => {
      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }
    };
  }, []);

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
        <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
          <div className="flex-1 flex justify-start">
            <Button
              variant="ghost"
              size="icon"
              onClick={onToggleSidebar}
              className={
                isNewChat
                  ? "text-muted-foreground hover:text-foreground"
                  : "text-muted-foreground hover:text-foreground hidden md:flex"
              }
              title={isSidebarOpen ? "Close sidebar" : "Open sidebar"}
            >
              <PanelLeft className="h-5 w-5" />
            </Button>
          </div>
          <div className="flex items-center gap-2 min-w-0">
            {!shouldShowWelcome &&
              (isEditingTitle ? (
                <Input
                  ref={titleInputRef}
                  value={titleInput}
                  onChange={(e) => setTitleInput(e.target.value)}
                  onBlur={handleTitleSubmit}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") handleTitleSubmit();
                    if (e.key === "Escape") {
                      setTitleInput(currentSession.title);
                      setIsEditingTitle(false);
                    }
                  }}
                  className="h-auto py-0 w-[300px] text-sm font-medium text-foreground text-center bg-transparent border-none focus-visible:ring-0 px-0"
                />
              ) : (
                <div
                  className="flex items-center gap-2 group cursor-pointer min-w-0"
                  onClick={() => setIsEditingTitle(true)}
                >
                  <span className="text-sm font-medium text-foreground truncate max-w-[300px]">
                    {currentSession.title}
                  </span>
                  <Pencil className="h-3 w-3 text-muted-foreground opacity-0 group-hover:opacity-100 transition-opacity" />
                </div>
              ))}
          </div>
          <div className="flex-1 flex justify-end gap-1">
            {!shouldShowWelcome && (
              <Button
                variant="ghost"
                size="sm"
                className="h-8 px-2 text-xs text-muted-foreground hover:text-foreground"
                onClick={() => {
                  const nextPlanState =
                    currentPlanState === "disabled"
                      ? "draft"
                      : currentPlanState === "draft"
                        ? "approved"
                        : "draft";
                  void handlePlanStateChange(nextPlanState);
                }}
                title={
                  currentPlanState === "disabled"
                    ? "Enter plan mode"
                    : currentPlanState === "draft"
                      ? "Approve plan"
                      : "Reopen planning"
                }
              >
                <FileText className="mr-1.5 h-4 w-4" />
                {currentPlanState === "disabled"
                  ? "Plan"
                  : currentPlanState === "draft"
                    ? "Approve"
                    : "Reopen"}
              </Button>
            )}
            {!shouldShowWelcome && (
              <Button
                variant="ghost"
                size="icon"
                className="text-muted-foreground hover:text-foreground"
                title="Share chat"
              >
                <Share2 className="h-5 w-5" />
              </Button>
            )}
          </div>
        </div>

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
          <div className="flex-1 min-h-0 flex flex-col items-center justify-center px-4 -mt-20">
            <div className="text-center mb-8">
              <h1 className="text-3xl font-semibold tracking-tight mb-2">
                What shall we think through?
              </h1>
              <p className="text-muted-foreground text-sm">
                Ask anything. I'm here to help you think, create, and explore.
              </p>
            </div>
          </div>
        ) : (
          <div
            ref={scrollContainerRef}
            onScroll={handleScroll}
            className="flex-1 min-h-0 overflow-y-auto"
          >
            <div
              ref={contentRef}
              className="max-w-3xl mx-auto px-4 space-y-6 pb-6 pt-4"
            >
              {messages.map((msg, index) => {
                const isLast = index === messages.length - 1;
                const retryFromUser = msg.role === "user" && isLast;
                const retryFromAssistant =
                  msg.role === "assistant" && !!msg.error;

                return (
                  <MessageItem
                    key={msg.id}
                    message={msg}
                    sessionId={currentSessionId ?? undefined}
                    canRetry={retryFromUser || retryFromAssistant}
                    onOpenPreview={handleOpenPreview}
                    onRetry={() => {
                      const batch = retryFromUser
                        ? [
                            {
                              content: msg.content,
                              attachments: msg.attachments ?? [],
                            },
                          ]
                        : collectRetryBatch(messages, msg.id);

                      void handleSend({
                        sessionIdHint: currentSessionId ?? undefined,
                        includeComposerInput: false,
                        retryAssistantId: retryFromAssistant
                          ? msg.id
                          : undefined,
                        overrideBatch: batch,
                        skipAppendUserMessages: true,
                      });
                    }}
                  />
                );
              })}
            </div>
          </div>
        )}

        {todoProgress &&
          todoProgress.items.length > 0 &&
          hasPendingAssistantMessage && (
            <div className="shrink-0 px-4 pb-2">
              <div className="max-w-3xl mx-auto">
                <div className="rounded-lg border border-border/60 bg-card/80 px-3 py-2.5 text-sm">
                  <div className="flex items-center gap-2 mb-2">
                    <div className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
                      <svg
                        className="h-3.5 w-3.5"
                        viewBox="0 0 16 16"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="1.5"
                      >
                        <rect x="2" y="2" width="12" height="12" rx="2" />
                        <path d="M5 8l2 2 4-4" />
                      </svg>
                      Progress
                    </div>
                    <span className="text-xs text-muted-foreground">
                      {todoProgress.completed}/{todoProgress.total}
                    </span>
                    <div className="flex-1 h-1.5 rounded-full bg-muted/50 overflow-hidden">
                      <div
                        className="h-full rounded-full bg-primary/70 transition-all duration-300"
                        style={{
                          width: `${todoProgress.total > 0 ? (todoProgress.completed / todoProgress.total) * 100 : 0}%`,
                        }}
                      />
                    </div>
                  </div>
                  <ul className="space-y-0.5">
                    {todoProgress.items.map((item, i) => (
                      <li
                        key={i}
                        className="flex items-start gap-1.5 text-xs leading-relaxed"
                      >
                        <span
                          className={`mt-0.5 shrink-0 ${item.checked ? "text-primary" : "text-muted-foreground/50"}`}
                        >
                          {item.checked ? "✓" : "○"}
                        </span>
                        <span
                          className={
                            item.checked
                              ? "text-muted-foreground line-through"
                              : "text-foreground"
                          }
                        >
                          {item.text}
                        </span>
                      </li>
                    ))}
                  </ul>
                </div>
              </div>
            </div>
          )}

        {showComposer && (
          <div className="shrink-0 px-4 pb-2 pt-2 bg-transparent w-full z-20 pointer-events-none">
            <div className="max-w-3xl mx-auto w-full pointer-events-auto">
              {pendingPermission && (
                <div
                  className={`mb-3 rounded-lg border px-3 py-2 text-sm ${permissionToneClass}`}
                >
                  <div className="font-medium">
                    Approval required ({pendingPermission.mode})
                  </div>
                  <div className="mt-1 break-all">
                    {pendingPermission.script}
                  </div>
                  <div className="mt-2 flex gap-2">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={isPermissionSubmitting}
                      onClick={() => void handlePermissionDecision(true)}
                    >
                      Approve
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={isPermissionSubmitting}
                      onClick={() => void handlePermissionDecision(false)}
                    >
                      Deny
                    </Button>
                  </div>
                </div>
              )}
              {pendingWorkspace && (
                <div
                  className={`mb-3 rounded-lg border px-3 py-2 text-sm ${workspaceToneClass}`}
                >
                  <div className="font-medium">Workspace access required</div>
                  <div className="mt-1 break-all font-mono text-xs">
                    {pendingWorkspace.path}
                  </div>
                  <div className="mt-2 text-muted-foreground">
                    {pendingWorkspace.reason}
                  </div>
                  <div className="mt-2 flex gap-2">
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={isWorkspaceSubmitting}
                      onClick={() => void handleWorkspaceDecision(true)}
                    >
                      Approve
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={isWorkspaceSubmitting}
                      onClick={() => void handleWorkspaceDecision(false)}
                    >
                      Deny
                    </Button>
                  </div>
                </div>
              )}
              {pendingAskUser && (
                <div className="mb-3 rounded-xl border-2 border-primary/40 bg-card px-4 py-3 shadow-xl">
                  <div className="flex items-center justify-between gap-3">
                    <div className="text-sm font-semibold text-foreground">
                      Input required
                    </div>
                    <div className="text-xs text-muted-foreground">
                      {pendingAskUser.request.questions.length} question
                      {pendingAskUser.request.questions.length > 1 ? "s" : ""}
                    </div>
                  </div>

                  <div className="mt-3 max-h-[36vh] overflow-y-auto space-y-3 pr-1">
                    {pendingAskUser.request.questions.map((question, idx) => {
                      const answer = pendingAskUser.answers[idx]?.answer;
                      const selected =
                        answer?.kind === "selected" ? answer.values : [];
                      const custom =
                        answer?.kind === "custom" ? answer.text : "";

                      return (
                        <div
                          key={`${question.section}-${idx}`}
                          className="rounded-lg border border-border bg-background p-3"
                        >
                          <div className="flex items-start justify-between gap-2">
                            <div className="text-sm font-medium text-foreground leading-snug">
                              {question.question}
                            </div>
                            {question.section ? (
                              <span className="text-[10px] uppercase tracking-wide text-muted-foreground border border-border rounded-full px-2 py-0.5">
                                {question.section}
                              </span>
                            ) : null}
                          </div>

                          {question.option.length > 0 && (
                            <div className="mt-2 flex flex-col gap-2">
                              {question.option.map((option) => {
                                const active = selected.includes(option);
                                return (
                                  <Button
                                    key={option}
                                    size="sm"
                                    variant={active ? "default" : "outline"}
                                    className="h-8 w-full justify-start"
                                    disabled={isAskUserSubmitting}
                                    onClick={() => {
                                      if (question.multiSelect) {
                                        const next = active
                                          ? selected.filter((v) => v !== option)
                                          : [...selected, option];
                                        handleAskUserAnswerChange(
                                          idx,
                                          next.length > 0
                                            ? {
                                                kind: "selected",
                                                values: next,
                                              }
                                            : { kind: "skipped" },
                                        );
                                      } else {
                                        handleAskUserAnswerChange(idx, {
                                          kind: "selected",
                                          values: [option],
                                        });
                                      }
                                    }}
                                  >
                                    {option}
                                  </Button>
                                );
                              })}
                            </div>
                          )}

                          <Input
                            value={custom}
                            onChange={(e) =>
                              handleAskUserAnswerChange(
                                idx,
                                e.target.value.trim().length > 0
                                  ? {
                                      kind: "custom",
                                      text: e.target.value,
                                    }
                                  : { kind: "skipped" },
                              )
                            }
                            placeholder="Custom answer (optional)"
                            className="mt-2 h-9 bg-background"
                            disabled={isAskUserSubmitting}
                          />
                        </div>
                      );
                    })}
                  </div>

                  <div className="mt-3 flex items-center justify-end gap-2">
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={isAskUserSubmitting}
                      onClick={() => {
                        if (!pendingAskUser) return;
                        const skipped = pendingAskUser.request.questions.map(
                          (q) => ({
                            section: q.section,
                            answer: { kind: "skipped" as const },
                          }),
                        );
                        void handleAskUserSubmit(skipped);
                      }}
                    >
                      Skip all
                    </Button>
                    <Button
                      size="sm"
                      disabled={isAskUserSubmitting}
                      onClick={() => void handleAskUserSubmit()}
                    >
                      Submit
                    </Button>
                  </div>
                </div>
              )}
              {uiError && (
                <div className="mb-3 rounded-lg border border-destructive bg-destructive px-3 py-2 text-sm text-destructive-foreground">
                  {uiError}
                </div>
              )}
              <div className="relative bg-background/60 backdrop-blur-xl rounded-2xl border border-border/50">
                {attachments.length > 0 && (
                  <div className="flex gap-2 p-3 pb-0 overflow-x-auto">
                    {attachments.map((att) => (
                      <div
                        key={att.id}
                        className="relative group flex-shrink-0"
                      >
                        <AttachmentPreview attachment={att}>
                          <AttachmentCard attachment={att} variant="composer" />
                        </AttachmentPreview>
                        <button
                          onClick={(e) => {
                            e.stopPropagation();
                            setAttachments((prev) =>
                              prev.filter((a) => a.id !== att.id),
                            );
                          }}
                          className="absolute -top-1 -right-1 bg-destructive text-destructive-foreground rounded-full p-0.5 shadow-sm opacity-0 group-hover:opacity-100 transition-opacity z-10"
                        >
                          <X className="h-3 w-3" />
                        </button>
                      </div>
                    ))}
                  </div>
                )}

                <PendingFollowUpPanel
                  messages={queuedMessages}
                  title={pendingPanelTitle}
                  subtitle={pendingPanelSubtitle}
                  sessionId={currentSessionId}
                  onSteerMessage={steerQueuedMessage}
                  onRemoveMessage={removeQueuedMessage}
                />

                <Textarea
                  ref={composerInputRef}
                  value={inputValue}
                  onChange={(e) => setInputValue(e.target.value)}
                  onPaste={handlePaste}
                  onKeyDown={(e) => {
                    if (e.key === "Tab" && isGenerating) {
                      e.preventDefault();
                      if (!canSend) return;
                      void handleSend({ deliveryMode: "queue" });
                      return;
                    }
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      if (!canSend) return;
                      void handleSend({ deliveryMode: "steer" });
                    }
                  }}
                  placeholder={composerPlaceholder}
                  className="min-h-[60px] max-h-[200px] w-full resize-none border-0 shadow-none focus-visible:ring-0 bg-transparent py-4 px-3 text-base"
                  rows={1}
                  autoFocus={isNewChat}
                />

                <div className="flex items-center justify-between p-2 pl-3">
                  <div className="flex items-center gap-2">
                    <input
                      type="file"
                      multiple
                      className="hidden"
                      ref={fileInputRef}
                      onChange={handleFileSelect}
                    />
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button
                          variant="ghost"
                          size="icon"
                          className="h-8 w-8 text-muted-foreground hover:text-foreground rounded-lg"
                        >
                          <Plus className="h-5 w-5" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="start" className="w-48">
                        <DropdownMenuItem
                          onClick={() => fileInputRef.current?.click()}
                        >
                          <File className="mr-2 h-4 w-4" />
                          Upload File
                        </DropdownMenuItem>
                        <DropdownMenuItem
                          onClick={() => fileInputRef.current?.click()}
                        >
                          <ImageIcon className="mr-2 h-4 w-4" />
                          Upload Photo
                        </DropdownMenuItem>
                        <DropdownMenuItem onClick={handleScreenshot}>
                          <Monitor className="mr-2 h-4 w-4" />
                          Take Screenshot
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  </div>
                  <div className="flex items-center gap-2">
                    <ModelSelector
                      currentModelId={currentModelId}
                      currentReasoningEffort={currentReasoningEffort}
                      onModelChange={(id) => {
                        if (isNewChat) {
                          setNewChatModelId(id);
                          setIsModelUpdating(true);
                          void setLastModel(id)
                            .catch((error) => {
                              setUiError(
                                `Failed to update default model: ${
                                  error instanceof Error
                                    ? error.message
                                    : String(error)
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
                                error instanceof Error
                                  ? error.message
                                  : String(error)
                              }`,
                            );
                          })
                          .finally(() => {
                            setIsModelUpdating(false);
                          });
                      }}
                      onReasoningEffortChange={(e) => {
                        if (isNewChat) {
                          setNewChatReasoningEffort(e);
                        } else {
                          updateSessionReasoningEffort(currentSessionId, e);
                        }
                      }}
                      disabled={isModelUpdating || isSending}
                      isUpdating={isModelUpdating}
                    />
                    {showTokenUsage && (
                      <ContextInspector
                        currentSessionId={currentSessionId}
                        promptTokens={latestStats?.promptTokens}
                        contextUsedTokens={contextUsedTokens}
                        contextWindow={contextWindow}
                        contextUsagePercent={contextUsagePercent}
                        contextUsageTitle={contextUsageTitle}
                      />
                    )}
                    {showStopButton ? (
                      <Button
                        onClick={handleStop}
                        size="icon"
                        className="h-8 w-8 bg-destructive text-destructive-foreground rounded-lg hover:bg-destructive/90"
                      >
                        <Square className="h-3.5 w-3.5 fill-current" />
                      </Button>
                    ) : (
                      <Button
                        onClick={() => void handleSend()}
                        disabled={!canSend}
                        size="icon"
                        className={
                          canSend
                            ? "h-8 w-8 bg-primary text-primary-foreground rounded-lg"
                            : "h-8 w-8 bg-muted text-muted-foreground rounded-lg"
                        }
                      >
                        <Send className="h-4 w-4" />
                      </Button>
                    )}
                  </div>
                </div>
              </div>
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
                onToggleHistory={() => setShowArtifactHistory((prev) => !prev)}
                onClose={() => {
                  setIsArtifactsOpen(false);
                  setShowArtifactHistory(false);
                }}
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
