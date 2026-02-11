import { useRef, useEffect, useState } from "react";
import { useStore } from "@/lib/store";
import {
  pollAskUserRequest,
  pollPermissionRequest,
  respondAskUserRequest,
  respondPermissionRequest,
  streamChat,
  type AskUserAnswer,
  type AskUserRequest,
} from "@/lib/api";
import { MessageItem } from "./MessageItem";
import { ModelSelector } from "./ModelSelector";
import { AttachmentPreview } from "./AttachmentPreview";

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
} from "lucide-react";
import type { Attachment } from "@/types";
import { v4 as uuidv4 } from "uuid";

interface QueuedMessage {
  content: string;
  attachments: Attachment[];
}

interface PendingPermission {
  id: string;
  mode: "sandboxed" | "network" | "unsafe";
  script: string;
}

interface AskUserState {
  request: AskUserRequest;
  answers: AskUserAnswer[];
}

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
    sessions,
    createSession,
    addMessage,
    replaceSessionMessages,
    updateSessionTitle,
    updateSessionModel,
    updateSessionReasoningEffort,
    saveFullConfig,
    uiConfig,
    models,
  } = useStore();
  const [inputValue, setInputValue] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleInput, setTitleInput] = useState("");
  const [isGenerating, setIsGenerating] = useState(false);
  const [queuedMessages, setQueuedMessages] = useState<QueuedMessage[]>([]);
  const [newChatModelId, setNewChatModelId] = useState("gpt-4");
  const [uiError, setUiError] = useState<string | null>(null);
  const [pendingPermission, setPendingPermission] =
    useState<PendingPermission | null>(null);
  const [isPermissionSubmitting, setIsPermissionSubmitting] = useState(false);
  const [pendingAskUser, setPendingAskUser] = useState<AskUserState | null>(null);
  const [isAskUserSubmitting, setIsAskUserSubmitting] = useState(false);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const userScrolledUpRef = useRef(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);
  const abortControllerRef = useRef<AbortController | null>(null);

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const messages = currentSession?.messages || [];
  const isNewChat = !currentSessionId || !currentSession;
  const hasInput = inputValue.trim() || attachments.length > 0;

  useEffect(() => {
    const persisted = uiConfig.fullConfig?.last_model;
    if (persisted && models.some((m) => m.id === persisted) && persisted !== newChatModelId) {
      setNewChatModelId(persisted);
      return;
    }

    if (models.length > 0 && !models.some((m) => m.id === newChatModelId)) {
      setNewChatModelId(models[0].id);
    }
  }, [models, newChatModelId, uiConfig.fullConfig?.last_model]);

  useEffect(() => {
    if (currentSession) setTitleInput(currentSession.title);
  }, [currentSession?.id, currentSession?.title]);
  useEffect(() => {
    if (isEditingTitle) titleInputRef.current?.focus();
  }, [isEditingTitle]);

  useEffect(() => {
    let cancelled = false;

    const pollPermissions = async () => {
      while (!cancelled) {
        if (pendingAskUser) {
          await new Promise((r) => setTimeout(r, 300));
          continue;
        }
        try {
          const req = await pollPermissionRequest();
          if (cancelled) break;
          if (req) {
            setPendingPermission(req);
          }
        } catch {
          // keep polling even if one request fails
        }
      }
    };

    const pollAskUser = async () => {
      while (!cancelled) {
        if (pendingPermission) {
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
          // keep polling even if one request fails
        }
      }
    };

    void pollPermissions();
    void pollAskUser();
    return () => {
      cancelled = true;
    };
  }, [pendingPermission, pendingAskUser]);
  // Auto-scroll: follow output unless user scrolled up
  // Auto-scroll via ResizeObserver: only scroll when content grows and user is near bottom
  const contentRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const container = scrollContainerRef.current;
    const content = contentRef.current;
    if (!container || !content) return;

    const observer = new ResizeObserver(() => {
      if (userScrolledUpRef.current) return;
      container.scrollTop = container.scrollHeight;
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [currentSessionId]);

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

  const handleSend = async () => {
    if (!inputValue.trim() && attachments.length === 0) return;

    setUiError(null);
    const content = inputValue;
    const atts = [...attachments];
    setInputValue("");
    setAttachments([]);

    if (isGenerating) {
      setQueuedMessages((prev) => [...prev, { content, attachments: atts }]);
      return;
    }

    let sessionId = currentSessionId;
    if (!sessionId) {
      sessionId = await createSession(newChatModelId);
    }

    if (!sessionId) return;

    const pendingBatch = [{ content, attachments: atts }, ...queuedMessages];
    setQueuedMessages([]);

    for (const queued of pendingBatch) {
      addMessage(sessionId, queued.content, "user", queued.attachments);
    }

    userScrolledUpRef.current = false;
    setIsGenerating(true);

    const controller = new AbortController();
    abortControllerRef.current = controller;

    try {
      const finalPrompt = pendingBatch.map((m) => m.content).join("\n\n");
      const mergedAttachments = pendingBatch.flatMap((m) => m.attachments);
      const assistantId = `assistant-${Date.now()}`;

      const current = useStore
        .getState()
        .sessions.find((s) => s.id === sessionId);
      if (!current) return;

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

      let streamedText = "";
      let streamedReasoning = "";

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
                ? { ...m, thinking: streamedReasoning, isThinking: true }
                : m,
            ),
          );
        },
        onText: (chunk) => {
          streamedText += chunk;
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
                    content: streamedText,
                    isStreaming: true,
                    isThinking: false,
                  }
                : m,
            ),
          );
        },
        onDone: (assistant) => {
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
                  }
                : m,
            ),
          );
        },
        onToolStart: (tool) => {
          const name = String((tool as { name?: string }).name ?? "bash");
          streamedReasoning += `\n\n$ ${name} running...`;
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? { ...m, thinking: streamedReasoning, isThinking: true }
                : m,
            ),
          );
        },
        onToolEnd: (tool) => {
          const name = String((tool as { name?: string }).name ?? "bash");
          streamedReasoning += `\n$ ${name} finished`;
          const latest = useStore
            .getState()
            .sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? { ...m, thinking: streamedReasoning, isThinking: true }
                : m,
            ),
          );
        },
        onError: (message) => {
          setUiError(`LLM endpoint error: ${message}`);
        },
      });
    } catch (error) {
      if (!(error instanceof DOMException && error.name === "AbortError")) {
        setUiError(`LLM endpoint error: ${(error as Error).message}`);
      }
    } finally {
      abortControllerRef.current = null;
      setIsGenerating(false);
    }
  };

  const handleStop = () => {
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    setIsGenerating(false);
  };

  const handlePermissionDecision = async (approved: boolean) => {
    if (!pendingPermission) return;
    setIsPermissionSubmitting(true);
    try {
      await respondPermissionRequest(pendingPermission.id, approved);
      setPendingPermission(null);
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
    try {
      await respondAskUserRequest(
        pendingAskUser.request.id,
        answersOverride ?? pendingAskUser.answers,
      );
      setPendingAskUser(null);
    } finally {
      setIsAskUserSubmitting(false);
    }
  };

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

  const handlePaste = (e: React.ClipboardEvent) => {
    const items = e.clipboardData.items;
    const newAttachments: Attachment[] = [];
    let hasHandledFile = false;

    for (const item of items) {
      if (item.kind === "file") {
        const file = item.getAsFile();
        if (!file) continue;
        hasHandledFile = true;
        const fileType = classifyFile(file);
        newAttachments.push({
          id: uuidv4(),
          name:
            file.name ||
            (fileType === "image" ? "Pasted Image" : "Pasted File"),
          type: fileType,
          url: URL.createObjectURL(file),
        });
      }
    }

    // Check for long text paste
    const text = e.clipboardData.getData("text/plain");
    if (!hasHandledFile && text && text.length >= LONG_TEXT_THRESHOLD) {
      e.preventDefault();
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
      if (hasHandledFile) e.preventDefault();
      setAttachments((prev) => [...prev, ...newAttachments]);
    }
  };


  const currentModelId = isNewChat ? newChatModelId : currentSession.model;
  const currentReasoningEffort = isNewChat
    ? undefined
    : currentSession.reasoningEffort;

  return (
    <div className="flex flex-col h-screen w-full relative">
      {/* Header */}
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
          {isNewChat ? (
            <span className="text-sm font-medium text-foreground">New Chat</span>
          ) : isEditingTitle ? (
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
          )}
        </div>
        <div className="flex-1 flex justify-end">
          {!isNewChat && (
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

      {/* Messages */}
      {isNewChat ? (
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
            {messages.map((msg) => (
              <MessageItem key={msg.id} message={msg} />
            ))}
          </div>
        </div>
      )}

      {/* Input Area */}
      <div className="p-4 bg-transparent absolute bottom-0 left-0 right-0 w-full z-20 pointer-events-none">
        <div className="max-w-3xl mx-auto w-full pointer-events-auto">
          {pendingPermission && (
            <div className="mb-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
              <div className="font-medium text-amber-200">Approval required ({pendingPermission.mode})</div>
              <div className="mt-1 text-amber-100/90 break-all">{pendingPermission.script}</div>
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
                        <div className="mt-2 flex flex-wrap gap-2">
                          {question.option.map((option) => {
                            const active = selected.includes(option);
                            return (
                              <Button
                                key={option}
                                size="sm"
                                variant={active ? "default" : "outline"}
                                className="h-8"
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
                      onClick={() =>
                        setQueuedMessages((prev) =>
                          prev.filter((_, idx) => idx !== i),
                        )
                      }
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
                    if (!isNewChat) {
                      updateSessionReasoningEffort(currentSessionId, e);
                    }
                  }}
                />
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
                    disabled={!hasInput}
                    size="icon"
                    className={
                      hasInput
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
  );
}
