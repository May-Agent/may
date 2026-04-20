import { useState } from 'react';
import { ChevronDown, Sparkles } from 'lucide-react';
import { cn } from '@/lib/utils';

interface ReasoningBlockProps {
  text: string;
  streaming?: boolean;
}

export function ReasoningBlock({ text, streaming }: ReasoningBlockProps) {
  const [open, setOpen] = useState(false);
  if (text.length === 0) return null;
  return (
    <div className="rounded-lg border border-dashed border-border/60 bg-muted/20 px-3 py-2">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-2 text-xs text-muted-foreground hover:text-foreground transition-colors w-full"
      >
        <Sparkles className={cn('size-3.5', streaming && 'animate-pulse')} />
        <span className={streaming ? 'shimmer-text font-medium' : 'font-medium'}>
          {streaming ? 'Thinking…' : 'Thought'}
        </span>
        <ChevronDown className={cn('size-3 ml-auto transition-transform', open && 'rotate-180')} />
      </button>
      {open && (
        <div className="mt-1.5 text-xs leading-relaxed text-muted-foreground whitespace-pre-wrap">
          {text}
        </div>
      )}
    </div>
  );
}
