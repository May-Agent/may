import { memo } from 'react';
import { Streamdown } from 'streamdown';

interface MessageTextProps {
  text: string;
  streaming?: boolean;
}

export const MessageText = memo(function MessageText({ text, streaming }: MessageTextProps) {
  if (text.length === 0) return null;
  return (
    <div className="prose prose-sm dark:prose-invert max-w-none prose-pre:bg-muted/60 prose-pre:border prose-pre:border-border prose-code:before:content-none prose-code:after:content-none prose-code:bg-muted/60 prose-code:rounded prose-code:px-1 prose-code:py-0.5 prose-code:text-xs">
      <Streamdown>{text}</Streamdown>
      {streaming && (
        <span className="inline-block w-1.5 h-3.5 bg-foreground/70 animate-pulse ml-0.5 align-middle" />
      )}
    </div>
  );
});
