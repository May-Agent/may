import React, { useRef, useEffect, useState } from "react";
import { useStore } from "@/lib/store";
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

const MOCK_RESPONSE = `Here is a simple example of a reusable Button component using TypeScript and Tailwind CSS:

\`\`\`tsx
import React from "react";

interface ButtonProps {
  variant?: "primary" | "secondary";
  children: React.ReactNode;
}

export const Button = ({ variant = "primary", children }: ButtonProps) => {
  return <button className={variant}>{children}</button>;
};
\`\`\`

This component accepts a \`variant\` prop and renders children inside a styled button. You can extend it with more variants, sizes, and other props as needed.`;

const MOCK_THINKING = `The user is asking for a reusable Button component. I should create something with TypeScript generics for type safety, Tailwind CSS classes for styling, and a clean prop interface.

Let me think about the variant pattern — a discriminated union would work well here. I could use a simple string union type like "primary" | "secondary" | "destructive" | "outline" | "ghost" for the variants.

For the styling approach, I'll use class-variance-authority (cva) to handle the variant styles cleanly. This gives us type-safe variant props with Tailwind classes mapped to each variant.

I should also consider the size prop — small, medium, large — and make the component extend native button attributes so it's fully compatible with HTML button props. I'll keep it simple but extensible.`;

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
    addMessage,
    updateMessageContent,
    setMessageStreaming,
    updateMessageThinking,
    setMessageThinkingState,
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
  const streamAbortRef = useRef(false);
  const streamingMsgIdRef = useRef<string | null>(null);
  const shouldFlushQueueRef = useRef(false);

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

  useEffect(() => {
    return () => {
      streamAbortRef.current = true;
    };
  }, [currentSessionId]);

  // Auto-send all queued messages at once when generation finishes normally
  useEffect(() => {
    if (!isGenerating && shouldFlushQueueRef.current && queuedMessages.length > 0 && currentSessionId) {
      shouldFlushQueueRef.current = false;
      const allQueued = [...queuedMessages];
      setQueuedMessages([]);

      // Add all queued messages as user messages
      for (const msg of allQueued) {
        addMessage(currentSessionId, msg.content, "user", msg.attachments);
      }

      // Start a single AI response
      setIsGenerating(true);
      streamAbortRef.current = false;
      const msgId = uuidv4();
      streamingMsgIdRef.current = msgId;
      useStore.setState((state) => ({
        sessions: state.sessions.map((s) =>
          s.id === currentSessionId
            ? {
                ...s,
                messages: [
                  ...s.messages,
                  {
                    id: msgId,
                    role: "assistant" as const,
                    content: "",
                    thinking: "",
                    isThinking: true,
                    timestamp: new Date().toISOString(),
                    isStreaming: true,
                  },
                ],
                lastModified: new Date().toISOString(),
              }
            : s,
        ),
      }));
      setTimeout(() => simulateStream(currentSessionId, msgId), 300);
    } else if (!isGenerating) {
      shouldFlushQueueRef.current = false;
    }
  }, [isGenerating]);

  const simulateStream = async (sessionId: string, messageId: string) => {
    // Phase 1: Thinking
    setMessageThinkingState(sessionId, messageId, true);
    const thinkTokens = MOCK_THINKING.split(/(?<=\s)/);
    let thinkAcc = "";
    for (const token of thinkTokens) {
      if (streamAbortRef.current) break;
      thinkAcc += token;
      updateMessageThinking(sessionId, messageId, thinkAcc);
      await new Promise((r) => setTimeout(r, 30 + Math.random() * 40));
    }
    if (streamAbortRef.current) {
      setMessageThinkingState(sessionId, messageId, false);
      setMessageStreaming(sessionId, messageId, false);
      streamingMsgIdRef.current = null;
      setIsGenerating(false);
      return;
    }
    setMessageThinkingState(sessionId, messageId, false);

    // Phase 2: Content streaming
    await new Promise((r) => setTimeout(r, 200));
    const tokens = MOCK_RESPONSE.split(/(?<=\s)/);
    let accumulated = "";
    for (const token of tokens) {
      if (streamAbortRef.current) break;
      accumulated += token;
      updateMessageContent(sessionId, messageId, accumulated);
      await new Promise((r) => setTimeout(r, 30 + Math.random() * 40));
    }
    setMessageStreaming(sessionId, messageId, false);
    streamingMsgIdRef.current = null;
    shouldFlushQueueRef.current = true;
    setIsGenerating(false);
  };

  const handleSend = () => {
    if (!currentSessionId || (!inputValue.trim() && attachments.length === 0))
      return;
    const content = inputValue;
    const atts = [...attachments];
    setInputValue("");
    setAttachments([]);

    if (isGenerating) {
      setQueuedMessages((prev) => [...prev, { content, attachments: atts }]);
      return;
    }

    addMessage(currentSessionId, content, "user", atts);
    userScrolledUpRef.current = false;
    setIsGenerating(true);
    streamAbortRef.current = false;

    // Create an empty assistant message with streaming flag
    const msgId = uuidv4();
    streamingMsgIdRef.current = msgId;

    // Add empty streaming message to store
    const { sessions } = useStore.getState();
    const session = sessions.find((s) => s.id === currentSessionId);
    if (!session) return;

    // Manually add the streaming message
    useStore.setState((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === currentSessionId
          ? {
              ...s,
              messages: [
                ...s.messages,
                {
                  id: msgId,
                  role: "assistant" as const,
                  content: "",
                  thinking: "",
                  isThinking: true,
                  timestamp: new Date().toISOString(),
                  isStreaming: true,
                },
              ],
              lastModified: new Date().toISOString(),
            }
          : s,
      ),
    }));

    // Start streaming after a small delay (simulating network latency)
    setTimeout(() => simulateStream(currentSessionId, msgId), 300);
  };

  const handleStop = () => {
    streamAbortRef.current = true;
    shouldFlushQueueRef.current = false;
    if (streamingMsgIdRef.current && currentSessionId) {
      setMessageThinkingState(currentSessionId, streamingMsgIdRef.current, false);
      setMessageStreaming(currentSessionId, streamingMsgIdRef.current, false);
      streamingMsgIdRef.current = null;
    }
    // Flush queued messages as user messages without triggering AI response
    if (currentSessionId && queuedMessages.length > 0) {
      for (const msg of queuedMessages) {
        addMessage(currentSessionId, msg.content, "user", msg.attachments);
      }
      setQueuedMessages([]);
    }
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
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground">
        Select a chat to start messaging
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
                  handleSend();
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
                    onClick={handleSend}
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
