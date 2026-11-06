import { useState } from 'react';
import { Languages } from 'lucide-react';
import type { ReplyTranslation } from '@/lib/types';
import { useTranslation } from '@/lib/i18n';
import { MessageText } from './MessageText';

interface TranslatedTextProps {
  /** The reply as the model wrote it. */
  text: string;
  /** Its display translation; without one the reply renders as is. */
  translation?: ReplyTranslation | null;
  streaming?: boolean;
  sessionId?: string;
}

/**
 * A reply, or its translation under a "translated" marker when the reply was
 * not in the communication language. The marker toggles the original.
 */
export function TranslatedText({ text, translation, streaming, sessionId }: TranslatedTextProps) {
  const { t } = useTranslation();
  const [showOriginal, setShowOriginal] = useState(false);
  if (!translation) {
    return <MessageText text={text} streaming={streaming} sessionId={sessionId} />;
  }
  return (
    <div className="space-y-1">
      <button
        type="button"
        data-testid="translation-marker"
        aria-pressed={showOriginal}
        onClick={() => setShowOriginal((open) => !open)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground hover:text-foreground"
      >
        <Languages className="size-3.5" />
        <span>
          {showOriginal
            ? t('chat.originalReply')
            : t('chat.translatedTo', { language: translation.languageName })}
        </span>
        <span aria-hidden>·</span>
        <span className="underline underline-offset-2">
          {showOriginal ? t('chat.showTranslation') : t('chat.showOriginal')}
        </span>
      </button>
      <MessageText
        text={showOriginal ? text : translation.text}
        streaming={streaming}
        sessionId={sessionId}
      />
    </div>
  );
}
