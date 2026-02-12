import { memo, useState, useMemo, useEffect } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { motion, AnimatePresence } from 'framer-motion';
import { cn } from '@/lib/utils';
import type { Message } from '@/types';
import ReactMarkdown from 'react-markdown';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { Copy, Check, FileText, File, ChevronRight, RotateCcw, Download, Terminal, Shield, Globe, AlertTriangle, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';
import { ThinkingBlock } from './ThinkingBlock';
import { FlowMarkdown } from './StreamingText';

function extractPreviewPaths(content: string): string[] {
  return Array.from(
    content.matchAll(/<artifact\s+url=["']([^"']+)["']\s*\/?>/gi),
  )
    .map((m) => m[1]?.trim())
    .filter((v): v is string => !!v);
}

function stripPreviewTags(content: string): string {
  return content
    .replace(/\s*<artifact\s+url=["'][^"']+["']\s*\/?>\s*/gi, '\n')
    .trim();
}

const MemoMarkdown = memo(function MemoMarkdown({
  content,
  components,
}: {
  content: string;
  components: any;
}) {
  return <ReactMarkdown components={components}>{content}</ReactMarkdown>;
});

async function copyWithRichText(markdown: string) {
  const plain = markdown ?? '';

  const html = renderToStaticMarkup(
    <ReactMarkdown>{plain}</ReactMarkdown>,
  );

  if (typeof ClipboardItem !== 'undefined' && navigator.clipboard?.write) {
    const item = new ClipboardItem({
      'text/plain': new Blob([plain], { type: 'text/plain' }),
      'text/html': new Blob([html], { type: 'text/html' }),
    });
    await navigator.clipboard.write([item]);
    return;
  }

  await navigator.clipboard.writeText(plain);
}

function CopyButton({ text, className }: { text: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button variant="ghost" size="icon" className={className}
      onClick={async () => {
        await copyWithRichText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 2000);
      }}>
      {copied ? <Check className="h-3 w-3 text-green-400" /> : <Copy className="h-3 w-3" />}
    </Button>
  );
}

function detectCodeLanguage(text: string): 'bash' | 'json' | 'text' {
  const trimmed = text.trim();
  if (!trimmed) return 'text';
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try {
      JSON.parse(trimmed);
      return 'json';
    } catch {
      // fall through
    }
  }
  if (/(\$\s+|bash|zsh|sh\s)/i.test(trimmed)) return 'bash';
  return 'text';
}

function formatCodeText(text: string, language: 'bash' | 'json' | 'text'): string {
  if (language !== 'json') return text;
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function modeConfig(mode?: string) {
  switch (mode) {
    case 'network':
      return { icon: Globe, color: 'text-blue-400', bg: 'bg-blue-500/10', border: 'border-blue-500/20', label: 'network' };
    case 'unsafe':
      return { icon: AlertTriangle, color: 'text-red-400', bg: 'bg-red-500/10', border: 'border-red-500/20', label: 'unsafe' };
    default:
      return { icon: Shield, color: 'text-emerald-400', bg: 'bg-emerald-500/10', border: 'border-emerald-500/20', label: 'sandboxed' };
  }
}

function makeComponents(isUser: boolean, isStreaming: boolean) {
  return {
    code({ className, children, ...props }: any) {
      const lang = /language-(\w+)/.exec(className || '')?.[1];
      const code = String(children).replace(/\n$/, '');
      return lang ? (
        <div className="relative group/code my-3 rounded-lg overflow-hidden border border-border/30">
          <div className="flex items-center justify-between px-4 py-1.5 bg-zinc-800 text-zinc-400 text-xs">
            <span>{lang}</span>
            {!isStreaming && <CopyButton text={code} className="h-5 w-5 text-zinc-400 hover:text-zinc-100 hover:bg-zinc-700/50" />}
          </div>
          {/* @ts-ignore */}
          <SyntaxHighlighter {...props} style={vscDarkPlus} language={lang} PreTag="div" customStyle={{ margin: 0, borderRadius: 0, fontSize: '0.8125rem' }}>
            {code}
          </SyntaxHighlighter>
        </div>
      ) : (
        <code {...props} className={cn('rounded px-1 py-0.5 font-mono text-sm', isUser ? 'bg-black/10 dark:bg-white/10' : 'bg-muted', className)}>{children}</code>
      );
    },
    p: ({ children }: any) => <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>,
    ul: ({ children }: any) => <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>,
    ol: ({ children }: any) => <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>,
  };
}

export function MessageItem({
  message,
  onRetry,
  canRetry,
  onOpenPreview,
}: {
  message: Message;
  onRetry?: () => void;
  canRetry?: boolean;
  onOpenPreview?: (path: string) => void;
}) {
  const isUser = message.role === 'user';
  const isStreaming = !!message.isStreaming;
  const hasThinking = !!message.thinking;
  const bashExecutions = message.bashExecutions || [];
  const [isBashOpen, setIsBashOpen] = useState(false);
  const [showTyping, setShowTyping] = useState(false);
  const components = useMemo(
    () => makeComponents(isUser, isStreaming),
    [isUser, isStreaming]
  );

  useEffect(() => {
    setIsBashOpen(false);
  }, [message.id]);

  useEffect(() => {
    if (!isStreaming || message.content.length > 0 || !!message.error) {
      setShowTyping(false);
      return;
    }

    const timer = window.setTimeout(() => setShowTyping(true), 1000);
    return () => window.clearTimeout(timer);
  }, [isStreaming, message.content, message.id, message.error]);

  const previewPaths = useMemo(() => extractPreviewPaths(message.content || ''), [message.content]);
  const displayContent = useMemo(() => stripPreviewTags(message.content || ''), [message.content]);

  useEffect(() => {
    if (isUser || !onOpenPreview || previewPaths.length === 0) return;
    onOpenPreview(previewPaths[previewPaths.length - 1]);
  }, [isUser, onOpenPreview, previewPaths]);

  const runningBashCount = bashExecutions.filter((exec) => exec.status === 'running').length;
  const failedBashCount = bashExecutions.filter((exec) => exec.status === 'failed').length;
  const nowMs = Date.now();
  const bashElapsedMs = bashExecutions.reduce((sum, exec) => {
    const startedAt = exec.startedAt ?? new Date(message.timestamp).getTime();
    const endedAt = exec.endedAt ?? (exec.status === 'running' ? nowMs : startedAt);
    const delta = Math.max(0, endedAt - startedAt);
    return sum + delta;
  }, 0);
  const bashElapsedSeconds = Math.max(1, Math.round(bashElapsedMs / 1000));

  return (
    <motion.div
      initial={{ opacity: 0, y: 16, scale: 0.97 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      transition={{ type: 'spring', stiffness: 500, damping: 30 }}
      className={cn('flex w-full gap-2 p-2 group/message', isUser ? 'flex-row-reverse' : 'flex-row')}>
      <div className={cn('flex flex-col relative', isUser ? 'max-w-[75%] items-end' : 'max-w-[85%] items-start')}>
        <div className={cn(
          'prose prose-sm dark:prose-invert max-w-none break-words relative prose-pre:p-0 prose-pre:m-0 prose-pre:bg-transparent',
          isUser
            ? 'px-4 py-2.5 rounded-2xl bg-primary text-primary-foreground rounded-br-md'
            : 'py-1 text-foreground w-full'
        )}>
          {!isUser && hasThinking && (
            <ThinkingBlock thinking={message.thinking!} isThinking={!!message.isThinking} />
          )}

          {!isUser && bashExecutions.length > 0 && !message.toolEvent && (
            <div className="mb-2 not-prose">
              <button
                type="button"
                className="inline-flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-[12px] text-muted-foreground hover:text-foreground hover:bg-muted/50 transition-all duration-200"
                onClick={() => setIsBashOpen((prev) => !prev)}
              >
                <Terminal className="h-3.5 w-3.5 shrink-0" />
                <span className="font-medium">{bashElapsedSeconds}s</span>
                <span className="text-muted-foreground/70">&middot;</span>
                <span className={cn(
                  "text-[11px]",
                  failedBashCount > 0 ? 'text-red-400' : runningBashCount > 0 ? 'text-blue-400' : 'text-muted-foreground/70'
                )}>
                  {runningBashCount > 0
                    ? `${runningBashCount} running`
                    : failedBashCount > 0
                      ? `${failedBashCount} failed`
                      : `${bashExecutions.length} cmd${bashExecutions.length > 1 ? 's' : ''}`}
                </span>
                <motion.div
                  animate={{ rotate: isBashOpen ? 90 : 0 }}
                  transition={{ duration: 0.15, ease: 'easeOut' }}
                >
                  <ChevronRight className="h-3 w-3 text-muted-foreground/50" />
                </motion.div>
              </button>

              <AnimatePresence initial={false}>
                {isBashOpen && (
                  <motion.div
                    initial={{ height: 0, opacity: 0 }}
                    animate={{ height: 'auto', opacity: 1 }}
                    exit={{ height: 0, opacity: 0 }}
                    transition={{ duration: 0.2, ease: [0.16, 1, 0.3, 1] }}
                    className="overflow-hidden"
                  >
                    <div className="mt-1.5 rounded-xl bg-zinc-900/60 backdrop-blur-xl backdrop-saturate-150 border border-white/[0.06] max-h-[400px] overflow-y-auto scrollbar-none">
                      <div className="divide-y divide-white/[0.04]">
                        {bashExecutions.map((exec, index) => {
                          const mode = modeConfig(exec.mode);
                          const ModeIcon = mode.icon;
                          const outputLang = detectCodeLanguage(exec.output || '');
                          const formattedOutput = formatCodeText(exec.output || '', outputLang);

                          return (
                            <motion.div
                              key={exec.id}
                              initial={{ opacity: 0 }}
                              animate={{ opacity: 1 }}
                              transition={{ duration: 0.15, delay: index * 0.02 }}
                              className="px-3 py-2"
                            >
                              <div className="flex items-center gap-2">
                                <ModeIcon className={cn('h-3 w-3 shrink-0', mode.color)} />
                                <span className="text-[11px] text-zinc-500 font-mono">$</span>
                                <span className="text-[11px] text-zinc-300 font-mono flex-1 break-all">{exec.input}</span>
                                {exec.status === 'running' ? (
                                  <Loader2 className="h-3 w-3 text-blue-400 animate-spin shrink-0" />
                                ) : exec.status === 'failed' ? (
                                  <span className="h-1.5 w-1.5 rounded-full bg-red-400 shrink-0" />
                                ) : (
                                  <span className="h-1.5 w-1.5 rounded-full bg-emerald-400 shrink-0" />
                                )}
                              </div>
                              {formattedOutput && (
                                <pre className="mt-1 text-[11px] leading-relaxed text-zinc-500 font-mono whitespace-pre-wrap break-all max-h-[150px] overflow-y-auto scrollbar-none pl-5">
                                  {formattedOutput}
                                </pre>
                              )}
                            </motion.div>
                          );
                        })}
                      </div>
                    </div>
                  </motion.div>
                )}
              </AnimatePresence>
            </div>
          )}

          {message.toolEvent && !isUser && (
            <div className="not-prose mb-2 rounded-md border border-border/40 bg-muted/20 px-3 py-2 text-xs text-muted-foreground font-mono whitespace-pre-wrap">
              {message.toolEvent.stage === 'start'
                ? `tool start: ${message.toolEvent.name ?? 'unknown'}`
                : `tool end: ${message.toolEvent.name ?? 'unknown'}`}
            </div>
          )}

          {message.error && !isUser && (
            <div className="not-prose mt-1 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <div>{message.error}</div>
              {onRetry && (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  className="mt-2 h-7 px-2 text-xs"
                  onClick={() => onRetry()}
                >
                  <RotateCcw className="mr-1 h-3 w-3" />
                  Retry
                </Button>
              )}
            </div>
          )}

          {displayContent && (
            isUser ? (
              <MemoMarkdown content={displayContent} components={components} />
            ) : (
              <FlowMarkdown content={displayContent} isStreaming={isStreaming} />
            )
          )}

          {!isUser && previewPaths.length > 0 && (
            <div className="not-prose mt-3 space-y-2">
              {previewPaths.map((path) => (
                <div key={path} className="group flex items-center justify-between rounded-xl border border-border/70 bg-card px-3 py-2">
                  <button
                    type="button"
                    onClick={() => onOpenPreview?.(path)}
                    className="min-w-0 flex-1 text-left"
                  >
                    <div className="truncate text-sm font-medium text-foreground">{path}</div>
                    <div className="text-xs text-muted-foreground">Preview</div>
                  </button>
                  <a
                    href={path.startsWith('http://') || path.startsWith('https://') ? path : '#'}
                    target="_blank"
                    rel="noreferrer"
                    onClick={(e) => {
                      if (!(path.startsWith('http://') || path.startsWith('https://'))) {
                        e.preventDefault();
                        onOpenPreview?.(path);
                      }
                    }}
                    className="ml-3 inline-flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
                    title="Open"
                  >
                    <Download className="h-4 w-4" />
                  </a>
                </div>
              ))}
            </div>
          )}
          {!isUser && showTyping && !message.content && !message.error && (
            <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.2s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.1s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce" />
              <span className="ml-1">typing</span>
            </div>
          )}

        </div>

        <div className={cn('flex items-center gap-2 mt-1 px-1', isUser ? 'flex-row-reverse' : 'flex-row')}>
          <span className="text-[10px] text-muted-foreground">{new Date(message.timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
          {!isStreaming && (
            <CopyButton
              text={message.content}
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
            />
          )}
          {isUser && canRetry && onRetry && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
              onClick={() => onRetry()}
            >
              <RotateCcw className="h-3 w-3" />
            </Button>
          )}
        </div>

        {message.attachments?.length ? (
          <div className={cn('flex flex-wrap gap-2 mt-2', isUser ? 'justify-end' : 'justify-start')}>
            {message.attachments.map(att => (
              <AttachmentPreview key={att.id} attachment={att}>
                {att.type === 'image' ? (
                  <div className="border rounded-md overflow-hidden bg-background">
                    <img src={att.url} alt={att.name} className="h-32 w-32 object-cover transition-transform hover:scale-105" />
                  </div>
                ) : (att.type === 'document' || att.type === 'pdf') ? (
                  <div className="h-20 w-48 border rounded-lg overflow-hidden bg-muted/40 p-2.5 flex flex-col gap-1">
                    <div className="flex items-center gap-1.5">
                      <FileText className={cn("h-3.5 w-3.5 flex-shrink-0", att.type === 'pdf' ? 'text-red-400' : 'text-blue-400')} />
                      <span className="text-[11px] font-medium truncate">{att.name}</span>
                    </div>
                    {att.textPreview ? (
                      <p className="text-[10px] text-muted-foreground/70 leading-tight line-clamp-3">{att.textPreview}</p>
                    ) : (
                      <p className="text-[10px] text-muted-foreground/50 uppercase tracking-wide mt-auto">{att.type === 'pdf' ? 'PDF' : 'DOCUMENT'}</p>
                    )}
                  </div>
                ) : (
                  <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted flex flex-col items-center justify-center p-2 gap-1">
                    <File className="h-6 w-6 text-muted-foreground" />
                    <span className="text-[9px] truncate w-full text-center" title={att.name}>{att.name}</span>
                  </div>
                )}
              </AttachmentPreview>
            ))}
          </div>
        ) : null}
      </div>
    </motion.div>
  );
}
