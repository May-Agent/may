import React, { useRef, useEffect, useState } from "react";
import { useStore } from "@/lib/store";
import { streamChat } from '@/lib/api';
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
  } = useStore();
  const [inputValue, setInputValue] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleInput, setTitleInput] = useState("");
  const [isGenerating, setIsGenerating] = useState(false);
  const [queuedMessages, setQueuedMessages] = useState<QueuedMessage[]>([]);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const userScrolledUpRef = useRef(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);
  const abortControllerRef = useRef<AbortController | null>(null);

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const messages = currentSession?.messages || [];

  useEffect(() => {
    if (currentSession) setTitleInput(currentSession.title);
  }, [currentSession?.id, currentSession?.title]);
  useEffect(() => {
    if (isEditingTitle) titleInputRef.current?.focus();
  }, [isEditingTitle]);
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
      sessionId = await createSession();
    }

    if (!sessionId) return;

    const pendingBatch = [{ content, attachments: atts }, ...queuedMessages];
    setQueuedMessages([]);

    for (const queued of pendingBatch) {
      addMessage(sessionId, queued.content, 'user', queued.attachments);
    }

    userScrolledUpRef.current = false;
    setIsGenerating(true);

    const controller = new AbortController();
    abortControllerRef.current = controller;

    try {
      const finalPrompt = pendingBatch.map((m) => m.content).join('\n\n');
      const mergedAttachments = pendingBatch.flatMap((m) => m.attachments);
      const assistantId = `assistant-${Date.now()}`;

      const current = useStore.getState().sessions.find((s) => s.id === sessionId);
      if (!current) return;

      replaceSessionMessages(sessionId, [
        ...current.messages,
        {
          id: assistantId,
          role: 'assistant',
          content: '',
          timestamp: new Date().toISOString(),
          attachments: [],
          isStreaming: true,
          isThinking: true,
          thinking: '',
        },
      ]);

      let streamedText = '';
      let streamedReasoning = '';

      await streamChat({
        sessionId,
        content: finalPrompt,
        attachments: mergedAttachments,
        signal: controller.signal,
        onReasoning: (chunk) => {
          streamedReasoning += chunk;
          const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId ? { ...m, thinking: streamedReasoning, isThinking: true } : m,
            ),
          );
        },
        onText: (chunk) => {
          streamedText += chunk;
          const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
          if (!latest) return;
          replaceSessionMessages(
            sessionId,
            latest.messages.map((m) =>
              m.id === assistantId
                ? { ...m, content: streamedText, isStreaming: true, isThinking: false }
                : m,
            ),
          );
        },
        onDone: (assistant) => {
          const latest = useStore.getState().sessions.find((s) => s.id === sessionId);
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
        onError: (message) => {
          addMessage(sessionId, `Error: ${message}`, 'assistant');
        },
      });
    } catch (error) {
      if (!(error instanceof DOMException && error.name === 'AbortError')) {
        addMessage(sessionId, `Error: ${(error as Error).message}`, 'assistant');
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
      file.type === "application/vnd.openxmlformats-officedocument.wordprocessingml.document" ||
      file.name.endsWith(".docx")
    )
      return "document";
    if (file.type === "text/plain" || file.name.endsWith(".txt") || file.name.endsWith(".md"))
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
          name: file.name || (fileType === "image" ? "Pasted Image" : "Pasted File"),
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

  if (!currentSessionId || !currentSession) {
    const hasWelcomeInput = inputValue.trim() || attachments.length > 0;
    return (
      <div className="flex flex-col h-screen w-full relative">
        <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
          <div className="flex-1 flex justify-start">
            <Button variant="ghost" size="icon" onClick={onToggleSidebar} className="text-muted-foreground hover:text-foreground">
              <PanelLeft className="h-5 w-5" />
            </Button>
          </div>
          <span className="text-sm font-medium">New Chat</span>
          <div className="flex-1" />
        </div>

        <div className="flex-1 flex flex-col items-center justify-center px-4 -mt-20">
          <div className="text-center mb-8">
            <h1 className="text-3xl font-semibold tracking-tight mb-2">What shall we think through?</h1>
            <p className="text-muted-foreground text-sm">Ask anything. I'm here to help you think, create, and explore.</p>
          </div>

          <div className="w-full max-w-2xl">
            <div className="relative bg-background/60 backdrop-blur-xl rounded-2xl border border-border/50 shadow-lg">
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
                placeholder="How can I help you today?"
                className="min-h-[60px] max-h-[200px] w-full resize-none border-0 shadow-none focus-visible:ring-0 bg-transparent py-4 px-3 text-base"
                rows={1}
                autoFocus
              />
              <div className="flex items-center justify-between p-2 pl-3">
                <div className="flex items-center gap-2">
                  <input type="file" multiple className="hidden" ref={fileInputRef} onChange={handleFileSelect} />
                  <Button variant="ghost" size="icon" className="h-8 w-8 text-muted-foreground hover:text-foreground rounded-lg" onClick={() => fileInputRef.current?.click()}>
                    <Plus className="h-5 w-5" />
                  </Button>
                </div>
                <div className="flex items-center gap-2">
                  <ModelSelector
                    currentModelId="claude-3-opus"
                    onModelChange={() => {}}
                    onReasoningEffortChange={() => {}}
                  />
                  <Button
                    onClick={() => void handleSend()}
                    disabled={!hasWelcomeInput}
                    size="icon"
                    className={hasWelcomeInput
                      ? "h-8 w-8 bg-primary text-primary-foreground rounded-lg"
                      : "h-8 w-8 bg-muted text-muted-foreground rounded-lg"}
                  >
                    <Send className="h-4 w-4" />
                  </Button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  const hasInput = inputValue.trim() || attachments.length > 0;

  return (
    <div className="flex flex-col h-screen w-full relative">
      {/* Header */}
      <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
        <div className="flex-1 flex justify-start">
          <Button
            variant="ghost"
            size="icon"
            onClick={onToggleSidebar}
            className="text-muted-foreground hover:text-foreground hidden md:flex"
            title={isSidebarOpen ? "Close sidebar" : "Open sidebar"}
          >
            <PanelLeft className="h-5 w-5" />
          </Button>
        </div>
        <div className="flex items-center gap-2 min-w-0">
          {isEditingTitle ? (
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
          <Button
            variant="ghost"
            size="icon"
            className="text-muted-foreground hover:text-foreground"
            title="Share chat"
          >
            <Share2 className="h-5 w-5" />
          </Button>
        </div>
      </div>

      {/* Messages */}
      <div
        ref={scrollContainerRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto"
      >
        <div ref={contentRef} className="max-w-3xl mx-auto px-4 space-y-6 pb-[200px] pt-4">
          {messages.map((msg) => (
            <MessageItem key={msg.id} message={msg} />
          ))}

        </div>
      </div>

      {/* Input Area */}
      <div className="p-4 bg-transparent absolute bottom-0 left-0 right-0 w-full z-20 pointer-events-none">
        <div className="max-w-3xl mx-auto w-full pointer-events-auto">
          <div className="relative bg-background/60 backdrop-blur-xl rounded-2xl border border-border/50 shadow-lg">
            {attachments.length > 0 && (
              <div className="flex gap-2 p-3 pb-0 overflow-x-auto">
                {attachments.map((att) => (
                  <div key={att.id} className="relative group flex-shrink-0">
                    <AttachmentPreview attachment={att}>
                      {att.type === "image" ? (
                        <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted">
                          <img src={att.url} alt={att.name} className="h-full w-full object-cover" />
                        </div>
                      ) : att.type === "document" || att.type === "pdf" ? (
                        <div className="h-20 w-44 border rounded-lg overflow-hidden bg-muted/50 p-2.5 flex flex-col gap-1">
                          <div className="flex items-center gap-1.5">
                            <FileText className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
                            <span className="text-[11px] font-medium truncate">{att.name}</span>
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
                        setAttachments((prev) => prev.filter((a) => a.id !== att.id));
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
              placeholder="Type a message..."
              className="min-h-[60px] max-h-[200px] w-full resize-none border-0 shadow-none focus-visible:ring-0 bg-transparent py-4 px-3 text-base"
              rows={1}
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
                  currentModelId={currentSession.model}
                  currentReasoningEffort={currentSession.reasoningEffort}
                  onModelChange={(id) =>
                    updateSessionModel(currentSessionId, id)
                  }
                  onReasoningEffortChange={(e) =>
                    updateSessionReasoningEffort(currentSessionId, e)
                  }
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
