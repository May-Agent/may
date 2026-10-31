import { useState } from 'react';
import { Check, Copy } from 'lucide-react';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

/**
 * Tiny muted actions beside a message, shown on hover or keyboard focus
 * (always on touch screens).
 */
export function MessageActions({ text, className }: { text: string; className?: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  if (text.trim().length === 0) return null;

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard access denied: nothing to report beyond the unchanged icon.
    }
  };

  const label = copied ? t('chat.copied') : t('chat.copy');
  return (
    <div
      className={cn(
        'flex items-center gap-0.5 opacity-0 transition-opacity group-hover/message:opacity-100 focus-within:opacity-100',
        className,
      )}
    >
      <button
        type="button"
        onClick={() => void copy()}
        aria-label={label}
        title={label}
        className="flex size-6 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
      >
        {copied ? <Check className="size-3.5" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
      </button>
    </div>
  );
}
