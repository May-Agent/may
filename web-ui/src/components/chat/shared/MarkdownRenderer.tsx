import { Suspense, lazy, memo } from 'react';
import { Streamdown } from 'streamdown';
import { cn } from '@/lib/utils';
import { CopyButton } from './CopyButton';

const LazySyntaxCodeBlock = lazy(() => import('../SyntaxCodeBlock'));

export const MemoMarkdown = memo(function MemoMarkdown({
  content,
  components,
}: {
  content: string;
  components: any;
}) {
  return (
    <Streamdown
      mode="static"
      parseIncompleteMarkdown={false}
      linkSafety={{ enabled: false }}
      controls={{ code: false, table: false }}
      components={components}
    >
      {content}
    </Streamdown>
  );
});

export function makeMarkdownComponents(isUser: boolean, isStreaming: boolean) {
  return {
    code({ className, children, ...props }: any) {
      const lang = /language-(\w+)/.exec(className || '')?.[1];
      const code = String(children).replace(/\n$/, '');
      return lang ? (
        <div className="relative group/code my-3 rounded-lg overflow-hidden border border-border/30">
          <div className="flex items-center justify-between px-4 py-1.5 bg-muted text-muted-foreground text-xs">
            <span>{lang}</span>
            {!isStreaming && (
              <CopyButton
                text={code}
                className="h-5 w-5 text-muted-foreground hover:text-foreground hover:bg-background/80"
              />
            )}
          </div>
          <Suspense
            fallback={
              <pre className="m-0 overflow-x-auto bg-background px-4 py-3 text-[0.8125rem] text-foreground">
                <code>{code}</code>
              </pre>
            }
          >
            <LazySyntaxCodeBlock language={lang} code={code} />
          </Suspense>
        </div>
      ) : (
        <code
          {...props}
          className={cn(
            'rounded px-1 py-0.5 font-mono text-sm',
            isUser ? 'bg-black/10 dark:bg-white/10' : 'bg-muted',
            className,
          )}
        >
          {children}
        </code>
      );
    },
    p: ({ children }: any) => (
      <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>
    ),
    ul: ({ children }: any) => (
      <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>
    ),
    ol: ({ children }: any) => (
      <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>
    ),
  };
}
