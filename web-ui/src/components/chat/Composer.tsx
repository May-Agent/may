import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowUp, Square } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

interface ComposerProps {
  disabled?: boolean;
  streaming?: boolean;
  onSend: (text: string) => void;
  onCancel: () => void;
}

export function Composer({ disabled, streaming, onSend, onCancel }: ComposerProps) {
  const [value, setValue] = useState('');
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = '0px';
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [value]);

  const send = useCallback(() => {
    const trimmed = value.trim();
    if (!trimmed || disabled) return;
    onSend(trimmed);
    setValue('');
  }, [value, disabled, onSend]);

  return (
    <div className="rounded-2xl border border-border bg-card shadow-sm">
      <textarea
        ref={textareaRef}
        className="block w-full resize-none bg-transparent px-4 pt-3 pb-1 text-sm outline-none placeholder:text-muted-foreground min-h-[48px] max-h-[240px] scrollbar-thin"
        placeholder={streaming ? 'May is responding…' : 'Message May'}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            send();
          }
        }}
      />
      <div className="flex items-center justify-between px-2 pb-2">
        <div className="text-[11px] text-muted-foreground px-2">
          Enter to send · Shift+Enter for newline
        </div>
        {streaming ? (
          <Button size="icon" variant="outline" onClick={onCancel} aria-label="Stop">
            <Square className="size-3.5 fill-current" />
          </Button>
        ) : (
          <Button
            size="icon"
            onClick={send}
            disabled={disabled || value.trim().length === 0}
            aria-label="Send"
            className={cn('transition-opacity', value.trim().length === 0 && 'opacity-50')}
          >
            <ArrowUp className="size-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
