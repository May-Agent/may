import { useRef, useEffect, useState, useCallback } from "react";
import { useNavigate } from "react-router-dom";
import { useStore } from "@/lib/store";
import {
  fetchArtifactPreview,
  fetchSessionArtifacts,
  fetchSessionRuntime,
  pollAskUserRequest,
  pollPermissionRequest,
  respondAskUserRequest,
  respondPermissionRequest,
  streamChat,
  subscribeSessionArtifacts,
  type AskUserAnswer,
  type AskUserRequest,
  type PermissionRequest,
} from "@/lib/api";
import { MessageItem } from "./MessageItem";
import { ModelSelector } from "./ModelSelector";
import { AttachmentPreview } from "./AttachmentPreview";
import { ArtifactsSidebar } from "./ArtifactsSidebar";

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
  Eye,
} from "lucide-react";
import type { Artifact, Attachment, Message } from "@/types";
import { v4 as uuidv4 } from "uuid";
import { useRuntimeStore } from "@/lib/runtimeStore";

interface QueuedMessage {
  content: string;
  attachments: Attachment[];
}

function collectRetryBatch(messages: Message[], assistantId: string): QueuedMessage[] {
  const index = messages.findIndex((m) => m.id === assistantId);
  if (index <= 0) return [];

  const batch: QueuedMessage[] = [];
  for (let i = index - 1; i >= 0; i--) {
    const msg = messages[i];
    if (msg.role !== "user") break;
    batch.unshift({
      content: msg.content,
      attachments: msg.attachments ?? [],
    });
  }
  return batch;
}

type QueueMap = Record<string, QueuedMessage[]>;

interface AskUserState {
  request: AskUserRequest;
  answers: AskUserAnswer[];
}

interface ChatInterfaceProps {
  onToggleSidebar: () => void;
  isSidebarOpen: boolean;
}

type PreviewPaneState = {
  kind: 'iframe' | 'image' | 'audio' | 'markdown' | 'text' | 'external';
  url?: string;
  markdown?: string;
  text?: string;
};

export function ChatInterface({
  onToggleSidebar,
  isSidebarOpen,
}: ChatInterfaceProps) {
  const navigate = useNavigate();
  const {
    currentSessionId,
    sessions,
    createSession,
    addMessage,
    replaceSessionMessages,
    updateSessionTitle,
    updateSessionModel,
    updateSessionReasoningEffort,
    saveFullConfig,
    uiConfig,
  } = useStore();
  const [inputValue, setInputValue] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleInput, setTitleInput] = useState("");
  const [queuedBySession, setQueuedBySession] = useState<QueueMap>({});
  const [newChatModelId, setNewChatModelId] = useState("");
  const [newChatReasoningEffort, setNewChatReasoningEffort] = useState<string | undefined>(undefined);
  const [uiError, setUiError] = useState<string | null>(null);
  const [pendingPermission, setPendingPermission] =
    useState<PermissionRequest | null>(null);
  const [isPermissionSubmitting, setIsPermissionSubmitting] = useState(false);
  const [pendingAskUser, setPendingAskUser] = useState<AskUserState | null>(null);
  const [isAskUserSubmitting, setIsAskUserSubmitting] = useState(false);
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [isArtifactsOpen, setIsArtifactsOpen] = useState(false);
  const [selectedPreviewPath, setSelectedPreviewPath] = useState<string | null>(null);
  const [previewPane, setPreviewPane] = useState<PreviewPaneState | null>(null);
  const previewBlobUrlRef = useRef<string | null>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const userScrolledUpRef = useRef(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);
  const abortControllerRef = useRef<AbortController | null>(null);
  const queuedBySessionRef = useRef<QueueMap>({});

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const messages = currentSession?.messages || [];
  const isNewChat = !currentSessionId || !currentSession;
  const shouldShowWelcome = isNewChat || messages.length === 0;
  const hasInput = inputValue.trim() || attachments.length > 0;
  const activeQueueKey = currentSessionId ?? '__new__';
  const queuedMessages = queuedBySession[activeQueueKey] ?? [];
  const canSend = hasInput && (!isNewChat || !!newChatModelId);
  const showTokenUsage = uiConfig.fullConfig?.preferences.show_token_usage ?? true;
  const latestAssistantWithStats = [...messages]
    .reverse()
    .find((m) => m.role === "assistant" && m.stats);
  const latestStats = latestAssistantWithStats?.stats;
  const contextWindow = latestStats?.contextWindow;
  const totalTokens = latestStats?.totalTokens;
  const contextUsagePercent =
    contextWindow && typeof totalTokens === "number"
      ? Math.max(0, Math.min(100, Math.round((totalTokens / contextWindow) * 100)))
      : null;
  const contextUsageTitle = latestStats
    ? `Context usage\n${totalTokens ?? "-"}/${contextWindow ?? "-"} tokens (${contextUsagePercent ?? "-"}%)\nPrompt: ${latestStats.promptTokens ?? "-"}\nCompletion: ${latestStats.completionTokens ?? "-"}`
    : "No context usage data yet";

  const runtimeSession = useRuntimeStore((state) =>
    currentSessionId ? state.sessions[currentSessionId] : undefined,
  );
  const isGenerating = runtimeSession?.state === 'streaming';
  const setRuntimeStreaming = useRuntimeStore((state) => state.setStreaming);
  const setRuntimeQueuedCount = useRuntimeStore((state) => state.setQueuedCount);
  const setRuntimeWaitingPermission = useRuntimeStore(
    (state) => state.setWaitingPermission,
  );
  const setRuntimeServerState = useRuntimeStore((state) => state.setServerState);
  const markRuntimeFirstToken = useRuntimeStore((state) => state.markFirstToken);
  const clearRuntimeSession = useRuntimeStore((state) => state.clearSessionRuntime);

  const isPreviewUrl = useCallback((value: string): boolean => {
    return value.startsWith('http://') || value.startsWith('https://');
  }, []);

  const refreshArtifacts = useCallback(async () => {
    if (!currentSessionId) {
      setArtifacts([]);
      return;
    }
    try {
      const next = await fetchSessionArtifacts(currentSessionId);
      setArtifacts(next);
      if (next.length === 0) {
        setIsArtifactsOpen(false);
        setSelectedPreviewPath(null);
        return;
      }

      if (selectedPreviewPath && !isPreviewUrl(selectedPreviewPath)) {
        const exists = next.some((a) => a.path === selectedPreviewPath);
        if (!exists) {
          setSelectedPreviewPath(next[0].path);
        }
      } else if (!selectedPreviewPath) {
        setSelectedPreviewPath(next[0].path);
      }
    } catch {
      // keep UI stable on transient failures
    }
  }, [currentSessionId, selectedPreviewPath, isPreviewUrl]);

  const openPreview = useCallback(async (path: string) => {
    const target = path.trim();
    if (!target || !currentSessionId) return;

    setSelectedPreviewPath(target);
    setIsArtifactsOpen(true);

    if (previewBlobUrlRef.current) {
      URL.revokeObjectURL(previewBlobUrlRef.current);
      previewBlobUrlRef.current = null;
    }

    if (isPreviewUrl(target)) {
      setPreviewPane({ kind: 'external', url: target });
      return;
    }

    try {
      const response = await fetchArtifactPreview(currentSessionId, target);
      const contentType = (response.headers.get('content-type') || '').toLowerCase();
      const ext = target.split('.').pop()?.toLowerCase() || '';
      const arrayBuffer = await response.arrayBuffer();
      const bytes = new Uint8Array(arrayBuffer);

      const text =
        contentType.startsWith('text/') ||
        contentType.includes('json') ||
        contentType.includes('xml') ||
        ['md', 'markdown', 'txt', 'log', 'csv'].includes(ext)
          ? new TextDecoder().decode(bytes)
          : '';

      if (contentType.includes('markdown') || ['md', 'markdown'].includes(ext)) {
        setPreviewPane({ kind: 'markdown', markdown: text });
      } else if (contentType.startsWith('text/plain') || ['txt', 'log', 'csv', 'json', 'xml'].includes(ext)) {
        setPreviewPane({ kind: 'text', text });
      } else if (ext === 'docx' || contentType.includes('officedocument.wordprocessingml.document')) {
        const mammoth = await import('mammoth');
        const htmlResult = await mammoth.default.convertToHtml({ arrayBuffer });
        const htmlBlob = new Blob([htmlResult.value], { type: 'text/html;charset=utf-8' });
        const htmlUrl = URL.createObjectURL(htmlBlob);
        previewBlobUrlRef.current = htmlUrl;
        setPreviewPane({ kind: 'iframe', url: htmlUrl });
      } else {
        const blob = new Blob([bytes], { type: response.headers.get('content-type') || 'application/octet-stream' });
        const objectUrl = URL.createObjectURL(blob);
        previewBlobUrlRef.current = objectUrl;

        if (contentType.startsWith('image/')) {
          setPreviewPane({ kind: 'image', url: objectUrl });
        } else if (contentType.startsWith('audio/')) {
          setPreviewPane({ kind: 'audio', url: objectUrl });
        } else {
          setPreviewPane({ kind: 'iframe', url: objectUrl });
        }
      }
    } catch {
      setPreviewPane(null);
    }
  }, [currentSessionId, isPreviewUrl]);

  const reloadPreview = useCallback(async () => {
    if (!selectedPreviewPath) return;
    await openPreview(selectedPreviewPath);
  }, [selectedPreviewPath, openPreview]);

  useEffect(() => {
    queuedBySessionRef.current = queuedBySession;
  }, [queuedBySession]);

  useEffect(() => {
    const persisted = uiConfig.fullConfig?.last_model;
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
  }, [newChatModelId, uiConfig.fullConfig?.last_model, uiConfig.fullConfig?.models.advanced]);

  useEffect(() => {
    if (currentSession) setTitleInput(currentSession.title);
  }, [currentSession?.id, currentSession?.title]);
  useEffect(() => {
    if (isEditingTitle) titleInputRef.current?.focus();
  }, [isEditingTitle]);

  useEffect(() => {
    if (!currentSessionId) return;
    useRuntimeStore.getState().ensureSession(currentSessionId);
    void (async () => {
      try {
        const runtime = await fetchSessionRuntime(currentSessionId);
        if (runtime.state === 'streaming' || runtime.state === 'waiting_permission') {
          setRuntimeServerState(currentSessionId, runtime.state);
        }
      } catch {
        // polling loop will self-heal
      }
    })();
  }, [currentSessionId, setRuntimeServerState]);

  useEffect(() => {
    if (!currentSessionId) {
      setArtifacts([]);
      setIsArtifactsOpen(false);
      setSelectedPreviewPath(null);
      setPreviewPane(null);
      return;
    }

    void refreshArtifacts();
    const unsubscribe = subscribeSessionArtifacts(currentSessionId, () => {
      void refreshArtifacts();
    });

    return () => {
      unsubscribe();
    };
  }, [currentSessionId, refreshArtifacts]);

  useEffect(() => {
    if (!currentSessionId) {
      setPendingPermission(null);
      return;
    }

    let cancelled = false;

    const pollPermissions = async () => {
      while (!cancelled) {
        if (pendingAskUser) {
          await new Promise((r) => setTimeout(r, 300));
          continue;
        }
        try {
          const { request } = await pollPermissionRequest(currentSessionId);
          if (cancelled) break;
          if (request && request.session_id && request.session_id !== currentSessionId) {
            throw new Error(`permission request session mismatch: expected ${currentSessionId}, got ${request.session_id}`);
          }
          setPendingPermission(request);
          setRuntimeWaitingPermission(currentSessionId, request);
        } catch {
          // keep polling even if one request fails
        }
      }
    };

    void pollPermissions();
    return () => {
      cancelled = true;
    };
  }, [currentSessionId, pendingAskUser, setRuntimeServerState, setRuntimeWaitingPermission]);

  useEffect(() => {
    let cancelled = false;

    const pollAskUser = async () => {
      while (!cancelled) {
        if (pendingPermission || pendingAskUser) {
          await new Promise((r) => setTimeout(r, 300));
          continue;
        }
        try {
          const req = await pollAskUserRequest();
          if (cancelled) break;
          if (req) {
            setPendingAskUser({
              request: req,
              answers: req.questions.map((q) => ({ section: q.section, answer: null })),
            });
          }
        } catch {
          // Keep polling after transient failures, but avoid tight retry loops.
          await new Promise((r) => setTimeout(r, 500));
        }
      }
    };

    void pollAskUser();
    return () => {
      cancelled = true;
    };
  }, [pendingPermission, pendingAskUser]);
  // Auto-scroll: follow output unless user intentionally scrolled up.
  const contentRef = useRef<HTMLDivElement>(null);

  const scrollToBottom = () => {
    const container = scrollContainerRef.current;
    if (!container) return;
    container.scrollTop = container.scrollHeight;
  };

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
    const el = scrollContainerRef.current;
    if (!el) return;
    const distFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (distFromBottom > 150) {
      userScrolledUpRef.current = true;
    } else if (distFromBottom < 30) {
      userScrolledUpRef.current = false;
    }
  };


  const handleSend = async (options?: {
    sessionIdHint?: string;
    includeComposerInput?: boolean;
    retryAssistantId?: string;
    overrideBatch?: QueuedMessage[];
    skipAppendUserMessages?: boolean;
  }) => {
    const includeComposerInput = options?.includeComposerInput ?? true;
    const composerHasInput = inputValue.trim().length > 0 || attachments.length > 0;
    if (includeComposerInput && !composerHasInput) return;

    setUiError(null);

    const content = includeComposerInput ? inputValue : "";
    const atts = includeComposerInput ? [...attachments] : [];
    if (includeComposerInput) {
      setInputValue("");
      setAttachments([]);
    }

    const queueSourceKey = options?.sessionIdHint ?? currentSessionId ?? '__new__';
    const targetRuntime = options?.sessionIdHint
      ? useRuntimeStore.getState().sessions[options.sessionIdHint]
      : runtimeSession;
    const targetGenerating = targetRuntime?.state === 'streaming';

    if (targetGenerating) {
      if (!includeComposerInput) return;
      setQueuedBySession((prev) => {
        const list = prev[queueSourceKey] ?? [];
        const nextList = [...list, { content, attachments: atts }];
        const runtimeSessionId = options?.sessionIdHint ?? currentSessionId;
        if (runtimeSessionId) {
          setRuntimeQueuedCount(runtimeSessionId, nextList.length);
        }
        return {
          ...prev,
          [queueSourceKey]: nextList,
        };
      });
      return;
    }

    let sessionId = options?.sessionIdHint ?? currentSessionId;
    if (!sessionId) {
      sessionId = await createSession(newChatModelId || undefined);
      if (sessionId) {
        navigate(`/chat/${encodeURIComponent(sessionId)}`);
      }
    }

    if (!sessionId) return;

    const existingQueue = queuedBySessionRef.current[queueSourceKey] ?? [];
    const pendingBatch = options?.overrideBatch
      ? [...options.overrideBatch]
      : includeComposerInput
        ? [{ content, attachments: atts }, ...existingQueue]
        : [...existingQueue];

    if (pendingBatch.length === 0) return;

    setQueuedBySession((prev) => {
      const next = { ...prev };
      delete next[queueSourceKey];
      delete next[sessionId];
      return next;
    });
    setRuntimeQueuedCount(sessionId, 0);

    if (!options?.skipAppendUserMessages) {
      for (const queued of pendingBatch) {
        addMessage(sessionId, queued.content, "user", queued.attachments);
      }
    }

    userScrolledUpRef.current = false;
    setRuntimeStreaming(sessionId);

    const controller = new AbortController();
    abortControllerRef.current = controller;
    let completedNormally = false;
    const assistantId = options?.retryAssistantId ?? `assistant-${Date.now()}`;
    let textFlushTimer: number | null = null;

    try {

      const current = useStore
        .getState()
        .sessions.find((s) => s.id === sessionId);
      if (!current) return;

      const isRetry = !!options?.retryAssistantId;
      const retryBatch = isRetry
        ? collectRetryBatch(current.messages, assistantId)
        : [];
      const streamingBatch = isRetry
        ? retryBatch
        : pendingBatch;

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
          },
        ]);
      }

      let streamedText = "";
      let streamedReasoning = "";
      let bashExecutions: Array<{
        id: string;
        input: string;
        output?: string;
        status: "running" | "done" | "failed";
        startedAt?: number;
        endedAt?: number;
      }> = [];
      let lastTextFlushAt = 0;

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
        replaceSessionMessages(
          sessionId,
          latest.messages.map((m) =>
            m.id === assistantId
              ? {
                  ...m,
                  content: streamedText,
                  isStreaming: true,
                  isThinking: false,
                  bashExecutions,
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
          streamedReasoning += chunk;
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...m,
                    thinking: streamedReasoning,
                    isThinking: true,
                    bashExecutions,
                  }
                : m,
            ),
          );
        },
        onToolStart: (tool: any) => {
          const name = (tool.name ?? '').toLowerCase();
          if (name === 'bash') {
            let script = '(bash)';
            try {
              const args = typeof tool.arguments === 'string' ? JSON.parse(tool.arguments) : tool.arguments;
              script = args?.script ?? args?.command ?? '(bash)';
            } catch { /* keep default */ }
            bashExecutions.push({
              id: tool.id ?? `tool-${Date.now()}`,
              input: script,
              status: 'running',
              startedAt: Date.now(),
            });
            flushAssistantText(true);
          }
        },
        onToolEnd: (tool: any) => {
          const idx = bashExecutions.findIndex((e) => e.id === tool.id);
          if (idx !== -1) {
            let output: string | undefined;
            try {
              const result = typeof tool.result === 'string' ? JSON.parse(tool.result) : tool.result;
              output = result?.stdout?.content?.text ?? (typeof result === 'string' ? result : JSON.stringify(result));
            } catch { /* ignore */ }
            const raw = JSON.stringify(tool.result ?? '').toLowerCase();
            const failed = raw.includes('"err"') || raw.includes('error');
            bashExecutions[idx] = {
              ...bashExecutions[idx],
              output,
              status: failed ? 'failed' : 'done',
              endedAt: Date.now(),
            };
            flushAssistantText(true);
          }
        },
        onText: (chunk) => {
          streamedText += chunk;
          markRuntimeFirstToken(sessionId);
          flushAssistantText();
        },
        onDone: (assistant) => {
          completedNormally = true;
          if (textFlushTimer != null) {
            window.clearTimeout(textFlushTimer);
            textFlushTimer = null;
          }
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...assistant,
                    id: assistantId,
                    thinking: streamedReasoning,
                    isStreaming: false,
                    isThinking: false,
                    bashExecutions,
                  }
                : m,
            ),
          );
        },
        onError: (message) => {
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (latest) {
            replaceSessionMessages(
              sessionId,
              latest.messages.map((m) =>
                m.id === assistantId
                  ? {
                      ...m,
                      isStreaming: false,
                      isThinking: false,
                      error: `LLM endpoint error: ${message}`,
                    }
                  : m,
              ),
            );
          }
          setUiError(null);
        },
      });
    } catch (error) {
      if (!(error instanceof DOMException && error.name === "AbortError")) {
        const errText = `LLM endpoint error: ${(error as Error).message}`;
        const latest = useStore
          .getState()
          .sessions.find((s) => s.id === sessionId);
        if (latest) {
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? {
                    ...m,
                    isStreaming: false,
                    isThinking: false,
                    error: errText,
                  }
                : m,
            ),
          );
        }
        setUiError(null);
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
        replaceSessionMessages(
          sessionId,
          latest.messages.map((m) =>
            m.id === assistantId
              ? {
                  ...m,
                  isStreaming: false,
                  isThinking: false,
                }
              : m,
          ),
        );
      }

      abortControllerRef.current = null;
      clearRuntimeSession(sessionId);

      if (completedNormally) {
        const remaining = queuedBySessionRef.current[sessionId] ?? [];
        if (remaining.length > 0) {
          void handleSend({ sessionIdHint: sessionId, includeComposerInput: false });
        }
      }
    }
  };

  const handleStop = () => {
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    if (currentSessionId) {
      const latest = useStore
        .getState()
        .sessions.find((s) => s.id === currentSessionId);
      if (latest) {
        let stopped = false;
        replaceSessionMessages(
          currentSessionId,
          latest.messages.map((m) => {
            if (
              !stopped &&
              m.role === "assistant" &&
              (m.isStreaming || m.isThinking)
            ) {
              stopped = true;
              return {
                ...m,
                isStreaming: false,
                isThinking: false,
              };
            }
            return m;
          }),
        );
      }

      clearRuntimeSession(currentSessionId);
    }
  };

  const handlePermissionDecision = async (approved: boolean) => {
    if (!pendingPermission || !currentSessionId) return;
    setIsPermissionSubmitting(true);
    try {
      await respondPermissionRequest(pendingPermission.id, approved);
      setPendingPermission(null);
      setRuntimeWaitingPermission(currentSessionId, null);
    } finally {
      setIsPermissionSubmitting(false);
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

  const handleAskUserSubmit = async (answersOverride?: AskUserAnswer[]) => {
    if (!pendingAskUser) return;
    setIsAskUserSubmitting(true);
    setUiError(null);
    try {
      await respondAskUserRequest(
        pendingAskUser.request.id,
        answersOverride ?? pendingAskUser.answers,
      );
      setPendingAskUser(null);
    } catch (error) {
      setUiError(`ask_user submit failed: ${(error as Error).message}`);
    } finally {
      setIsAskUserSubmitting(false);
    }
  };

  const permissionToneClass = pendingPermission
    ? pendingPermission.mode === "unsafe"
      ? "border-red-300 bg-red-100 text-red-900 dark:border-red-800 dark:bg-red-950/95 dark:text-red-100"
      : pendingPermission.mode === "network"
        ? "border-yellow-300 bg-yellow-100 text-yellow-900 dark:border-yellow-800 dark:bg-yellow-950/95 dark:text-yellow-100"
        : "border-border bg-card text-foreground"
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
          id: uuidv4(),
          name: `screenshot-${Date.now()}.png`,
          type: "image",
          url: canvas.toDataURL("image/png"),
        },
      ]);
    } catch (e) {
      console.error("Screenshot failed:", e);
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
    return new Promise((resolve) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as string);
      reader.onerror = () => resolve("");
      reader.readAsText(file);
    });
  };

  const readFileAsDataUrl = async (file: File): Promise<string> => {
    return new Promise((resolve) => {
      const reader = new FileReader();
      reader.onload = () => resolve(typeof reader.result === "string" ? reader.result : "");
      reader.onerror = () => resolve("");
      reader.readAsDataURL(file);
    });
  };

  const processDocxFile = async (file: File): Promise<string> => {
    try {
      const mammoth = await import("mammoth");
      const arrayBuffer = await file.arrayBuffer();
      const result = await mammoth.default.extractRawText({ arrayBuffer });
      return result.value;
    } catch {
      return "";
    }
  };

  const handleFileSelect = async (e: React.ChangeEvent<HTMLInputElement>) => {
    if (!e.target.files?.length) return;
    const files = Array.from(e.target.files!);
    const newAttachments: Attachment[] = [];

    for (const f of files) {
      const fileType = classifyFile(f);
      const att: Attachment = {
        id: uuidv4(),
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
  };

  const LONG_TEXT_THRESHOLD = 500;

  const handlePaste = async (e: React.ClipboardEvent) => {
    const items = Array.from(e.clipboardData.items);
    const text = e.clipboardData.getData("text/plain");
    const hasFileItem = items.some((item) => item.kind === "file" && item.getAsFile());
    const shouldAttachLargeText = !hasFileItem && !!text && text.length >= LONG_TEXT_THRESHOLD;

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
        id: uuidv4(),
        name: file.name || (fileType === "image" ? "Pasted Image" : "Pasted File"),
        type: fileType,
        url: URL.createObjectURL(file),
        uploadDataUrl: await readFileAsDataUrl(file),
      };
      newAttachments.push(att);
    }

    if (shouldAttachLargeText) {
      const preview = text.slice(0, 200);
      newAttachments.push({
        id: uuidv4(),
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

  useEffect(() => {
    return () => {
      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }
    };
  }, []);

  return (
    <div className="flex h-screen w-full relative">
      <div className="flex min-w-0 flex-1 flex-col h-screen relative">
        <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
          <div className="flex-1 flex justify-start">
            <Button
              variant="ghost"
              size="icon"
              onClick={onToggleSidebar}
              className={isNewChat ? "text-muted-foreground hover:text-foreground" : "text-muted-foreground hover:text-foreground hidden md:flex"}
              title={isSidebarOpen ? "Close sidebar" : "Open sidebar"}
            >
              <PanelLeft className="h-5 w-5" />
            </Button>
          </div>
          <div className="flex items-center gap-2 min-w-0">
            {!shouldShowWelcome && (isEditingTitle ? (
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
          <div className="flex-1 flex justify-end">
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

        {shouldShowWelcome ? (
          <div className="flex-1 flex flex-col items-center justify-center px-4 -mt-20">
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
            className="flex-1 overflow-y-auto"
          >
            <div
              ref={contentRef}
              className="max-w-3xl mx-auto px-4 space-y-6 pb-[200px] pt-4"
            >
              {messages.map((msg, index) => {
                const isLast = index === messages.length - 1;
                const retryFromUser = msg.role === "user" && isLast;
                const retryFromAssistant = msg.role === "assistant" && !!msg.error;

                return (
                  <MessageItem
                    key={msg.id}
                    message={msg}
                    canRetry={retryFromUser || retryFromAssistant}
                    onOpenPreview={(path) => {
                      void openPreview(path);
                    }}
                    onRetry={() => {
                      const batch = retryFromUser
                        ? [{ content: msg.content, attachments: msg.attachments ?? [] }]
                        : collectRetryBatch(messages, msg.id);

                      void handleSend({
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
        )}

        <div className="p-4 bg-transparent absolute bottom-0 left-0 right-0 w-full z-20 pointer-events-none">
          <div className="max-w-3xl mx-auto w-full pointer-events-auto">
            {pendingPermission && (
            <div className={`mb-3 rounded-lg border px-3 py-2 text-sm ${permissionToneClass}`}>
              <div className="font-medium">Approval required ({pendingPermission.mode})</div>
              <div className="mt-1 break-all">{pendingPermission.script}</div>
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
            {pendingAskUser && (
            <div className="mb-3 rounded-xl border-2 border-primary/40 bg-card px-4 py-3 shadow-xl">
              <div className="flex items-center justify-between gap-3">
                <div className="text-sm font-semibold text-foreground">Input required</div>
                <div className="text-xs text-muted-foreground">
                  {pendingAskUser.request.questions.length} question{pendingAskUser.request.questions.length > 1 ? "s" : ""}
                </div>
              </div>

              <div className="mt-3 max-h-[36vh] overflow-y-auto space-y-3 pr-1">
                {pendingAskUser.request.questions.map((question, idx) => {
                  const answer = pendingAskUser.answers[idx]?.answer;
                  const selected = Array.isArray(answer) ? answer : [];
                  const custom = typeof answer === "string" ? answer : "";

                  return (
                    <div key={`${question.section}-${idx}`} className="rounded-lg border border-border bg-background p-3">
                      <div className="flex items-start justify-between gap-2">
                        <div className="text-sm font-medium text-foreground leading-snug">{question.question}</div>
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
                                  if (question.multi_select) {
                                    const next = active
                                      ? selected.filter((v) => v !== option)
                                      : [...selected, option];
                                    handleAskUserAnswerChange(idx, next.length > 0 ? next : null);
                                  } else {
                                    handleAskUserAnswerChange(idx, [option]);
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
                        onChange={(e) => handleAskUserAnswerChange(idx, e.target.value)}
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
                    const skipped = pendingAskUser.request.questions.map((q) => ({
                      section: q.section,
                      answer: null,
                    }));
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
            <div className="mb-3 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              {uiError}
            </div>
          )}
            <div className="relative bg-background/60 backdrop-blur-xl rounded-2xl border border-border/50">
            {attachments.length > 0 && (
              <div className="flex gap-2 p-3 pb-0 overflow-x-auto">
                {attachments.map((att) => (
                  <div key={att.id} className="relative group flex-shrink-0">
                    <AttachmentPreview attachment={att}>
                      {att.type === "image" ? (
                        <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted">
                          <img
                            src={att.url}
                            alt={att.name}
                            className="h-full w-full object-cover"
                          />
                        </div>
                      ) : att.type === "document" || att.type === "pdf" ? (
                        <div className="h-20 w-44 border rounded-lg overflow-hidden bg-muted/50 p-2.5 flex flex-col gap-1">
                          <div className="flex items-center gap-1.5">
                            <FileText className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
                            <span className="text-[11px] font-medium truncate">
                              {att.name}
                            </span>
                          </div>
                          {att.textPreview ? (
                            <p className="text-[10px] text-muted-foreground/70 leading-tight line-clamp-3 overflow-hidden">
                              {att.textPreview}
                            </p>
                          ) : (
                            <p className="text-[10px] text-muted-foreground/50 uppercase tracking-wide mt-auto">
                              {att.type === "pdf" ? "PDF" : "DOCUMENT"}
                            </p>
                          )}
                        </div>
                      ) : (
                        <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted flex items-center justify-center">
                          <File className="h-8 w-8 text-muted-foreground" />
                        </div>
                      )}
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

            {queuedMessages.length > 0 && (
              <div className="flex flex-col gap-1 px-3 pt-3 pb-1">
                <span className="text-[10px] text-muted-foreground/60">
                  queued ({queuedMessages.length})
                </span>
                {queuedMessages.map((msg, i) => (
                  <div
                    key={i}
                    className="flex items-center justify-between gap-2 text-xs text-muted-foreground"
                  >
                    <span className="truncate">{msg.content}</span>
                    <button
                      onClick={() => {
                        setQueuedBySession((prev) => {
                          const list = prev[activeQueueKey] ?? [];
                          const nextList = list.filter((_, idx) => idx !== i);
                          const next = { ...prev, [activeQueueKey]: nextList };
                          if (nextList.length === 0) {
                            delete next[activeQueueKey];
                          }
                          return next;
                        });
                        if (currentSessionId) {
                          const list = queuedBySessionRef.current[currentSessionId] ?? [];
                          const nextLen = Math.max(0, list.length - 1);
                          setRuntimeQueuedCount(currentSessionId, nextLen);
                        }
                      }}
                      className="flex-shrink-0 hover:text-foreground transition-colors"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </div>
                ))}
              </div>
            )}

            <Textarea
              value={inputValue}
              onChange={(e) => setInputValue(e.target.value)}
              onPaste={handlePaste}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  if (!canSend) return;
                  void handleSend();
                }
              }}
              placeholder={isNewChat ? "How can I help you today?" : "Type a message..."}
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
                      if (uiConfig.fullConfig) {
                        void saveFullConfig({ ...uiConfig.fullConfig, last_model: id });
                      }
                      return;
                    }
                    void updateSessionModel(currentSessionId, id);
                  }}
                  onReasoningEffortChange={(e) => {
                    if (isNewChat) {
                      setNewChatReasoningEffort(e);
                    } else {
                      updateSessionReasoningEffort(currentSessionId, e);
                    }
                  }}
                />
                {showTokenUsage && (
                  <div
                    className="inline-flex h-8 items-center gap-1 rounded-lg px-2 text-xs text-muted-foreground hover:text-foreground"
                    title={contextUsageTitle}
                  >
                    <Eye className="h-4 w-4" />
                    <span>{contextUsagePercent != null ? `${contextUsagePercent}%` : "--"}</span>
                  </div>
                )}
                {isGenerating ? (
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
    </div>

      {canShowArtifactsPanel && (
        <ArtifactsSidebar
          open={isArtifactsOpen}
          artifacts={artifacts}
          selectedPath={selectedPreviewPath}
          preview={previewPane}
          onClose={() => setIsArtifactsOpen(false)}
          onReload={() => {
            void reloadPreview();
          }}
          onSelect={(path) => {
            void openPreview(path);
          }}
        />
      )}
    </div>
  );
}
