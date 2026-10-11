import { memo, type HTMLAttributes, type ReactNode } from 'react';
import { Streamdown } from 'streamdown';
import { PrismLight as SyntaxHighlighter } from 'react-syntax-highlighter';
import bash from 'react-syntax-highlighter/dist/esm/languages/prism/bash';
import javascript from 'react-syntax-highlighter/dist/esm/languages/prism/javascript';
import typescript from 'react-syntax-highlighter/dist/esm/languages/prism/typescript';
import jsx from 'react-syntax-highlighter/dist/esm/languages/prism/jsx';
import tsx from 'react-syntax-highlighter/dist/esm/languages/prism/tsx';
import python from 'react-syntax-highlighter/dist/esm/languages/prism/python';
import rust from 'react-syntax-highlighter/dist/esm/languages/prism/rust';
import json from 'react-syntax-highlighter/dist/esm/languages/prism/json';
import css from 'react-syntax-highlighter/dist/esm/languages/prism/css';
import markdown from 'react-syntax-highlighter/dist/esm/languages/prism/markdown';
import oneLight from 'react-syntax-highlighter/dist/esm/styles/prism/one-light';
import { parseArtifactTags } from '@/lib/artifactTags';
import { SlidevArtifact } from './SlidevArtifact';

for (const [name, lang] of Object.entries({
  bash,
  sh: bash,
  shell: bash,
  js: javascript,
  javascript,
  ts: typescript,
  typescript,
  jsx,
  tsx,
  py: python,
  python,
  rust,
  rs: rust,
  json,
  css,
  md: markdown,
  markdown,
})) {
  SyntaxHighlighter.registerLanguage(name, lang);
}

interface MessageTextProps {
  text: string;
  streaming?: boolean;
  sessionId?: string;
}

const codeBlockStyle: React.CSSProperties = {
  margin: 0,
  padding: '0.65rem 0.85rem',
  background: 'hsl(var(--muted) / 0.5)',
  border: '1px solid hsl(var(--border))',
  borderRadius: 'var(--radius)',
  fontSize: '0.8125rem',
  lineHeight: '1.55',
};

interface FenceProps extends HTMLAttributes<HTMLElement> {
  children?: ReactNode;
}

function extractLanguage(className?: string): string | null {
  if (!className) return null;
  const m = /language-([a-zA-Z0-9_+-]+)/.exec(className);
  return m ? m[1]!.toLowerCase() : null;
}

function extractText(node: ReactNode): string {
  if (node == null || node === false) return '';
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(extractText).join('');
  if (typeof node === 'object' && node !== null && 'props' in node) {
    const el = node as unknown as { props?: { children?: ReactNode } };
    return extractText(el.props?.children);
  }
  return '';
}

function Pre({ children }: FenceProps) {
  // Streamdown routes fenced blocks through <pre><code class="language-xx">…</code></pre>.
  // Render our own minimal container so we don't inherit Streamdown's card chrome.
  const codeElement = (children as { props?: FenceProps })?.props;
  const lang = extractLanguage(codeElement?.className);
  const source = extractText(codeElement?.children).replace(/\n$/, '');

  if (lang) {
    return (
      <SyntaxHighlighter
        language={lang}
        style={oneLight}
        customStyle={codeBlockStyle}
        codeTagProps={{ style: { background: 'transparent', fontFamily: 'inherit' } }}
        PreTag="div"
        wrapLongLines
      >
        {source}
      </SyntaxHighlighter>
    );
  }
  return (
    <pre
      style={codeBlockStyle}
      className="overflow-x-auto whitespace-pre-wrap break-words font-mono"
    >
      {source}
    </pre>
  );
}

function InlineCode({ children, ...rest }: FenceProps) {
  return (
    <code
      {...rest}
      className="rounded bg-muted/60 px-1 py-0.5 text-[0.85em] font-mono text-foreground"
    >
      {children}
    </code>
  );
}

const streamdownComponents = {
  pre: Pre,
  code: InlineCode,
};

export const MessageText = memo(function MessageText({
  text,
  streaming,
  sessionId,
}: MessageTextProps) {
  // `<tasks />` is a control tag: the backend independently watches tasks.md
  // and the PlanBanner renders the canonical document. Do not expose the raw
  // XML marker as assistant prose.
  const withoutTasks = text.replace(/<tasks\s*\/>/gi, '');
  const { visibleText, artifacts } = parseArtifactTags(withoutTasks);
  if (visibleText.length === 0 && artifacts.length === 0 && !streaming) return null;
  return (
    <div>
      {visibleText.length > 0 && (
        <div className="prose prose-sm dark:prose-invert max-w-none">
          <Streamdown components={streamdownComponents}>{visibleText}</Streamdown>
        </div>
      )}
      {sessionId &&
        artifacts.map((artifact) => (
          <SlidevArtifact
            key={`${artifact.type}:${artifact.path}`}
            sessionId={sessionId}
            path={artifact.path}
          />
        ))}
      {streaming && (
        <span className="inline-block w-1.5 h-3.5 bg-foreground/70 animate-pulse ml-0.5 align-middle" />
      )}
    </div>
  );
});
