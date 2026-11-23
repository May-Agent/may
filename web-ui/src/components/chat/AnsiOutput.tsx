import { memo, useMemo } from 'react';
import { cn } from '@/lib/utils';
import { hasAnsiOrCarriageReturn, parseAnsiLines, type AnsiStyle } from '@/lib/ansi';

function styleProps(style: AnsiStyle): React.CSSProperties | undefined {
  if (
    !style.fg &&
    !style.bg &&
    !style.bold &&
    !style.dim &&
    !style.italic &&
    !style.underline
  ) {
    return undefined;
  }
  return {
    color: style.fg,
    backgroundColor: style.bg,
    fontWeight: style.bold ? 600 : undefined,
    opacity: style.dim ? 0.65 : undefined,
    fontStyle: style.italic ? 'italic' : undefined,
    textDecoration: style.underline ? 'underline' : undefined,
  };
}

export const AnsiOutput = memo(function AnsiOutput({
  text,
  tone = 'stdout',
  className,
}: {
  text: string;
  tone?: 'stdout' | 'stderr';
  className?: string;
}) {
  const needsAnsi = useMemo(() => hasAnsiOrCarriageReturn(text), [text]);
  const lines = useMemo(() => (needsAnsi ? parseAnsiLines(text) : null), [needsAnsi, text]);

  return (
    <pre
      className={cn(
        'whitespace-pre-wrap text-xs font-mono max-h-64 overflow-auto scrollbar-thin',
        tone === 'stderr' ? 'text-destructive/90' : 'text-foreground/80',
        className,
      )}
    >
      {lines === null
        ? text
        : lines.map((spans, row) => (
            <span key={row}>
              {row > 0 && '\n'}
              {spans.map((span, col) => {
                const inline = styleProps(span.style);
                return inline ? (
                  <span key={col} style={inline}>
                    {span.text}
                  </span>
                ) : (
                  span.text
                );
              })}
            </span>
          ))}
    </pre>
  );
});
