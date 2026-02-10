import { useState } from 'react';
import { cn } from '@/lib/utils';
import type { Message } from '@/types';
import ReactMarkdown from 'react-markdown';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { Copy, Check } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';

function CodeCopyButton({ code }: { code: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="absolute right-2 top-2 z-10 opacity-0 group-hover/code:opacity-100 transition-opacity">
      <Button
        variant="ghost"
        size="icon"
        className="h-6 w-6 text-zinc-400 hover:text-zinc-100 hover:bg-zinc-700/50"
        onClick={async () => {
          await navigator.clipboard.writeText(code);
          setCopied(true);
          setTimeout(() => setCopied(false), 2000);
        }}
      >
        {copied ? <Check className="h-3 w-3 text-green-400" /> : <Copy className="h-3 w-3" />}
      </Button>
    </div>
  );
}

interface MessageItemProps {
  message: Message;
}

export function MessageItem({ message }: MessageItemProps) {
  const isUser = message.role === 'user';
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    await navigator.clipboard.writeText(message.content);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div className={cn(
      "flex w-full gap-2 p-2 group/message",
      isUser ? "flex-row-reverse" : "flex-row"
    )}>
      <div className={cn(
        "flex flex-col relative",
        isUser ? "max-w-[75%] items-end" : "w-full items-start"
      )}>
        <div className={cn(
          "prose prose-sm dark:prose-invert max-w-none break-words relative",
          isUser 
            ? "px-4 py-2 rounded-2xl shadow-sm bg-primary text-primary-foreground rounded-br-sm" 
            : "px-0 py-2 bg-transparent text-foreground w-full"
        )}>
          <ReactMarkdown
            components={{
              code({node, className, children, ...props}) {
                const match = /language-(\w+)/.exec(className || '')
                return match ? (
                  <div className="relative group/code my-2 rounded-md overflow-hidden">
                    <CodeCopyButton code={String(children)} />
                    {/* @ts-ignore */}
                    <SyntaxHighlighter
                      {...props}
                      style={vscDarkPlus}
                      language={match[1]}
                      PreTag="div"
                      customStyle={{ margin: 0, borderRadius: 0 }}
                    >
                      {String(children).replace(/\n$/, '')}
                    </SyntaxHighlighter>
                  </div>
                ) : (
                  <code {...props} className={cn(
                    "rounded px-1 py-0.5 font-mono text-sm",
                    isUser ? "bg-black/10 dark:bg-white/10" : "bg-muted",
                    className
                  )}>
                    {children}
                  </code>
                )
              },
              p: ({children}) => <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>,
              ul: ({children}) => <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>,
              ol: ({children}) => <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>,
            }}
          >
            {message.content}
          </ReactMarkdown>
        </div>
        
        <div className={cn(
          "flex items-center gap-2 mt-1 px-1",
          isUser ? "flex-row-reverse" : "flex-row"
        )}>
          <span className="text-[10px] text-muted-foreground">
            {new Date(message.timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}
          </span>
          {!isUser && (
            <Button
              variant="ghost"
              size="icon"
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
              onClick={handleCopy}
            >
              {copied ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
            </Button>
          )}
        </div>

        {/* Attachments */}
        {message.attachments && message.attachments.length > 0 && (
          <div className={cn("flex flex-wrap gap-2 mt-2", isUser ? "justify-end" : "justify-start")}>
            {message.attachments.map((att) => (
              <AttachmentPreview key={att.id} attachment={att}>
                <div className="relative group border rounded-md overflow-hidden bg-background">
                  {att.type === 'image' ? (
                    <img 
                      src={att.url} 
                      alt={att.name} 
                      className="h-32 w-32 object-cover transition-transform hover:scale-105"
                    />
                  ) : (
                    <div className="h-24 w-24 flex flex-col items-center justify-center p-2 text-center gap-1">
                      <div className="h-8 w-8 bg-muted rounded flex items-center justify-center">
                         <span className="text-xs font-bold text-muted-foreground">FILE</span>
                      </div>
                      <span className="text-[10px] w-full truncate px-1" title={att.name}>
                        {att.name}
                      </span>
                    </div>
                  )}
                </div>
              </AttachmentPreview>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
