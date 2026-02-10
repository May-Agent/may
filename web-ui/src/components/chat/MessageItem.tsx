import { useState, useMemo, useEffect } from 'react';
import { motion } from 'framer-motion';
import { cn } from '@/lib/utils';
import type { Message } from '@/types';
import ReactMarkdown from 'react-markdown';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { Copy, Check, FileText, File } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';
import { ThinkingBlock } from './ThinkingBlock';

function CopyButton({ text, className }: { text: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button variant="ghost" size="icon" className={className}
      onClick={async () => { await navigator.clipboard.writeText(text); setCopied(true); setTimeout(() => setCopied(false), 2000); }}>
      {copied ? <Check className="h-3 w-3 text-green-400" /> : <Copy className="h-3 w-3" />}
    </Button>
  );
}

// Tokenizer that survives ReactMarkdown remounts by using external state
interface TokenEntry { text: string; key: number }
interface TokenStore { tokens: TokenEntry[]; fullText: string; counter: number }
const tokenStores = new Map<string, TokenStore>();

function animateTokens(storeId: string, text: string) {
  let store = tokenStores.get(storeId);
  if (!store) {
    store = { tokens: [], fullText: '', counter: 0 };
    tokenStores.set(storeId, store);
  }

  if (text.length > store.fullText.length && text.startsWith(store.fullText)) {
    const newContent = text.slice(store.fullText.length);
    store.tokens.push({ text: newContent, key: store.counter++ });
    store.fullText = text;
  } else if (text !== store.fullText) {
    store.tokens = [{ text, key: store.counter++ }];
    store.fullText = text;
  }

  return store.tokens.map((t) => (
    <span key={t.key} className="streaming-fade-in">{t.text}</span>
  ));
}

function extractText(node: React.ReactNode): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (!node) return '';
  if (Array.isArray(node)) return node.map(extractText).join('');
  if (typeof node === 'object' && 'props' in node) {
    return extractText((node as any).props.children);
  }
  return '';
}

function makeComponents(isUser: boolean, isStreaming: boolean, messageId: string) {
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
    p: isStreaming
      ? ({ children }: any) => {
          const text = extractText(children);
          // Use text content itself as a stable identifier
          // (paragraphs with different text get different stores)
          const storeId = `${messageId}-p-${text.slice(0, 20)}`;
          return (
            <p className="mb-1 last:mb-0 leading-relaxed">
              {animateTokens(storeId, text)}
            </p>
          );
        }
      : ({ children }: any) => <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>,
    ul: ({ children }: any) => <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>,
    ol: ({ children }: any) => <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>,
  };
}

export function MessageItem({ message }: { message: Message }) {
  const isUser = message.role === 'user';
  const isStreaming = !!message.isStreaming;
  const hasThinking = !!message.thinking;
  const components = useMemo(
    () => makeComponents(isUser, isStreaming, message.id),
    [isUser, isStreaming, message.id]
  );

  // Clean up token stores when streaming ends
  useEffect(() => {
    if (!isStreaming) {
      const prefix = `${message.id}-`;
      for (const key of tokenStores.keys()) {
        if (key.startsWith(prefix)) tokenStores.delete(key);
      }
    }
  }, [isStreaming, message.id]);

  return (
    <motion.div
      initial={{ opacity: 0, y: 16, scale: 0.97 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      transition={{ type: 'spring', stiffness: 500, damping: 30 }}
      className={cn('flex w-full gap-2 p-2 group/message', isUser ? 'flex-row-reverse' : 'flex-row')}>
      <div className={cn('flex flex-col relative', isUser ? 'max-w-[75%] items-end' : 'w-full items-start')}>
        <div className={cn(
          'prose prose-sm dark:prose-invert max-w-none break-words relative',
          isUser ? 'px-4 py-2.5 rounded-2xl bg-primary text-primary-foreground rounded-br-md' : 'px-1 py-2 bg-transparent text-foreground w-full'
        )}>
          {!isUser && hasThinking && (
            <ThinkingBlock thinking={message.thinking!} isThinking={!!message.isThinking} />
          )}
          {message.content && (
            <ReactMarkdown components={components}>{message.content}</ReactMarkdown>
          )}
        </div>

        <div className={cn('flex items-center gap-2 mt-1 px-1', isUser ? 'flex-row-reverse' : 'flex-row')}>
          <span className="text-[10px] text-muted-foreground">{new Date(message.timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
          {!isUser && !isStreaming && <CopyButton text={message.content} className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground" />}
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
