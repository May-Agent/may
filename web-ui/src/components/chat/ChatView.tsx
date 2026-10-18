import { useCallback, useEffect, useRef, useState } from 'react';
import { AlertCircle, AlertTriangle, ArrowDown, RotateCw, X } from 'lucide-react';
import { useNavigate } from 'react-router-dom';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTurnStore } from '@/lib/stores/turn';
import { useConfigStore } from '@/lib/stores/config';
import { ScrollArea } from '@/components/ui/scroll-area';
import { AssistantTurn } from './AssistantTurn';
import { UserMessage } from './UserMessage';
import { PersistedAssistant } from './PersistedAssistant';
import { Composer } from './Composer';
import { SessionHeader } from './SessionHeader';
import { BrowserTakeoverBanner } from './BrowserTakeoverBanner';
import { PlanBanner } from './PlanBanner';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import type { ApiAttachment } from '@/lib/types';
import { enqueueMessage } from '@/lib/api/chat';
import { isConfigUsable } from '@/lib/api/config';
import { cn } from '@/lib/utils';

const STICKY_BOTTOM_THRESHOLD_PX = 48;

export function ChatView({ sessionId }: { sessionId: string }) {
  const open = useSessionsStore((s) => s.open);
  const messages = useSessionsStore((s) => s.activeMessages);
  const active = useSessionsStore((s) => s.active);
  const appendMessage = useSessionsStore((s) => s.appendMessage);
  const activeTurn = useTurnStore((s) => s.active);
  const stopping = useTurnStore((s) => s.stopping);
  const lastTurn = useTurnStore((s) => s.last);
  const send = useTurnStore((s) => s.send);
  const cancel = useTurnStore((s) => s.cancel);
  const retry = useTurnStore((s) => s.retry);
  const dismissError = useTurnStore((s) => s.dismissError);
  const config = useConfigStore((s) => s.config);
  const configLoaded = config !== null;
  const navigate = useNavigate();
  const { t } = useTranslation();

  const scrollerRef = useRef<HTMLDivElement>(null);
  const stickyBottomRef = useRef(true);
  const [showJumpButton, setShowJumpButton] = useState(false);

  useEffect(() => {
    open(sessionId);
  }, [sessionId, open]);

  const handleScroll = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
    const atBottom = gap < STICKY_BOTTOM_THRESHOLD_PX;
    stickyBottomRef.current = atBottom;
    setShowJumpButton(!atBottom);
  }, []);

  // Auto-scroll only if the user is pinned to the bottom. Avoids the jank
  // of yanking the viewport away from the user mid-read.
  useEffect(() => {
    if (!stickyBottomRef.current) return;
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [
    messages.length,
    activeTurn?.text.length,
    activeTurn?.tools.length,
    activeTurn?.reasoning.length,
  ]);

  const scrollToBottom = useCallback(() => {
    const el = scrollerRef.current;
    if (!el) return;
    stickyBottomRef.current = true;
    el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
  }, []);

  if (!active || active.id !== sessionId) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        {t('common.loading')}
      </div>
    );
  }

  const streaming = activeTurn !== null || stopping;
  const configUsable = !configLoaded || isConfigUsable(config);
  const showDecodeSpeed = config?.fullConfig.preferences.showDecodeSpeed ?? true;
  const reasoningEnabled =
    typeof active.reasoningEffort === 'string' &&
    active.reasoningEffort.length > 0 &&
    active.reasoningEffort.toLowerCase() !== 'off';
  const lastTurnForActive =
    lastTurn && lastTurn.sessionId === active.id && !lastTurn.error ? lastTurn : null;

  const handleSend = async (
    content: string,
    attachments: ApiAttachment[],
    deliveryMode: 'steer' | 'queue',
  ) => {
    const userMessage = {
      id: `local-${Date.now()}`,
      role: 'user' as const,
      content,
      timestamp: new Date().toISOString(),
      attachments,
    };
    appendMessage(userMessage);
    // Sending a new message is an explicit user intent to follow the tail.
    stickyBottomRef.current = true;

    if (streaming) {
      await enqueueMessage({
        sessionId,
        content,
        attachments,
        deliveryMode,
      });
      return;
    }

    await send({ sessionId, content, attachments });
  };

  return (
    <div className="relative flex h-full flex-col">
      <SessionHeader session={active} />
      <PlanBanner
        sessionId={active.id}
        planState={active.runtime?.planState ?? 'disabled'}
      />
      <BrowserTakeoverBanner session={active} />
      <ScrollArea
        ref={scrollerRef}
        onScroll={handleScroll}
        className="flex-1 px-6 pt-6 pb-48"
      >
        <div className="mx-auto w-full max-w-3xl space-y-6">
          {messages.map((msg) => {
            if (msg.role === 'user') {
              return (
                <UserMessage key={msg.id} content={msg.content} attachments={msg.attachments} />
              );
            }
            if (lastTurnForActive && lastTurnForActive.assistantId === msg.id) {
              return (
                <AssistantTurn
                  key={msg.id}
                  turn={lastTurnForActive}
                  reasoningEnabled={reasoningEnabled}
                  showDecodeSpeed={showDecodeSpeed}
                />
              );
            }
            return <PersistedAssistant key={msg.id} message={msg} sessionId={active.id} />;
          })}
          {activeTurn && (
            <AssistantTurn
              turn={activeTurn}
              reasoningEnabled={reasoningEnabled}
              showDecodeSpeed={showDecodeSpeed}
            />
          )}
          {!activeTurn && lastTurn?.error && (
            <TurnErrorCard
              code={lastTurn.error.code}
              message={lastTurn.error.message}
              retryable={lastTurn.error.retryable}
              onRetry={() => void retry()}
              onDismiss={dismissError}
            />
          )}
        </div>
      </ScrollArea>

      <div
        className={cn(
          'pointer-events-none absolute inset-x-0 bottom-0 flex justify-center px-6 pb-5',
        )}
      >
        <div className="pointer-events-auto mx-auto w-full max-w-3xl space-y-2">
          {showJumpButton && (
            <div className="flex justify-center">
              <Button
                size="sm"
                variant="secondary"
                onClick={scrollToBottom}
                className="h-8 gap-1 rounded-full border border-border/60 bg-background/80 px-3 shadow-md backdrop-blur-xl hover:bg-background"
                aria-label={t('chat.scrollToBottom')}
              >
                <ArrowDown className="size-3.5" />
                <span className="text-xs">{t('chat.scrollToBottom')}</span>
              </Button>
            </div>
          )}
          {!configUsable && (
            <ConfigGuardCard
              onOpenSettings={() => navigate('/settings?section=providers')}
            />
          )}
          <Composer
            disabled={!configUsable}
            streaming={streaming}
            onSend={handleSend}
            onCancel={() => void cancel()}
            sessionId={active.id}
            currentModel={active.model}
            currentReasoningEffort={active.reasoningEffort}
          />
        </div>
      </div>
    </div>
  );
}

interface TurnErrorCardProps {
  code: string;
  message: string;
  retryable: boolean;
  onRetry: () => void;
  onDismiss: () => void;
}

function TurnErrorCard({ code, message, retryable, onRetry, onDismiss }: TurnErrorCardProps) {
  const { t } = useTranslation();
  return (
    <div className="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2.5 text-sm text-destructive">
      <AlertCircle className="mt-0.5 size-4 shrink-0" />
      <div className="flex-1 min-w-0">
        <div className="font-medium">{humanizeErrorCode(code)}</div>
        <div className="text-xs opacity-80 break-words">{message}</div>
      </div>
      <div className="flex items-center gap-1 shrink-0">
        {retryable && (
          <Button size="sm" variant="outline" onClick={onRetry} className="h-7 gap-1 text-xs">
            <RotateCw className="size-3" />
            {t('common.retry')}
          </Button>
        )}
        <Button
          size="icon"
          variant="ghost"
          onClick={onDismiss}
          className="size-7"
          aria-label={t('common.dismiss')}
        >
          <X className="size-3.5" />
        </Button>
      </div>
    </div>
  );
}

function humanizeErrorCode(code: string): string {
  return code.replaceAll('_', ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

interface ConfigGuardCardProps {
  onOpenSettings: () => void;
}

function ConfigGuardCard({ onOpenSettings }: ConfigGuardCardProps) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onOpenSettings}
      className="flex w-full items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/5 px-3 py-2.5 text-left text-sm text-amber-700 transition-colors hover:bg-amber-500/10 dark:text-amber-300"
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0" />
      <div className="flex-1 min-w-0">
        <div className="font-medium">{t('chat.configGuard.title')}</div>
        <div className="text-xs opacity-80">{t('chat.configGuard.body')}</div>
      </div>
      <span className="shrink-0 text-xs font-medium underline-offset-4 hover:underline">
        {t('chat.configGuard.openSettings')}
      </span>
    </button>
  );
}
