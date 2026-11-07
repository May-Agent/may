import { Fragment, useCallback, useEffect, useRef, useState } from 'react';
import { AlertCircle, AlertTriangle, ArrowDown, Loader2, Moon, RotateCw, X } from 'lucide-react';
import { useNavigate, useParams } from 'react-router-dom';
import { toast } from 'sonner';
import { isDraftSessionId, useSessionsStore } from '@/lib/stores/sessions';
import { profileSessionPath } from '@/lib/profile/active';
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
import { YoloToggle } from './YoloToggle';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import type { ApiAttachment, ApiMessage } from '@/lib/types';
import { enqueueMessage } from '@/lib/api/chat';
import { bashTranscript, runBash } from '@/lib/api/bash';
import { isConfigUsable } from '@/lib/api/config';
import { endSleep, parseSleepCommand, startSleep } from '@/lib/api/sleep';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { cn } from '@/lib/utils';
import { formatGroupTime, startsGroup } from '@/lib/chat/time';
import { chatRows, type ChatRow } from '@/lib/chat/rows';

const STICKY_BOTTOM_THRESHOLD_PX = 48;
const NO_MESSAGES: ApiMessage[] = [];

export function ChatView({ sessionId }: { sessionId: string }) {
  const open = useSessionsStore((s) => s.open);
  const messages = useSessionsStore((s) => s.messages[sessionId] ?? NO_MESSAGES);
  const active = useSessionsStore((s) => s.active);
  const appendMessage = useSessionsStore((s) => s.appendMessage);
  // "Today" for the time labels between message groups.
  const [now] = useState(() => new Date());
  const sessionTurn = useTurnStore((s) => s.turns[sessionId]);
  const activeTurn = sessionTurn?.active ?? null;
  const stopping = sessionTurn?.stopping ?? false;
  const lastTurn = sessionTurn?.last ?? null;
  const send = useTurnStore((s) => s.send);
  const cancel = useTurnStore((s) => s.cancel);
  const retry = useTurnStore((s) => s.retry);
  const dismissError = useTurnStore((s) => s.dismissError);
  const watchTurns = useTurnStore((s) => s.watch);
  const config = useConfigStore((s) => s.config);
  const configLoaded = config !== null;
  const asleep = useRuntimeStore((s) => s.sleep?.asleep ?? false);
  const navigate = useNavigate();
  const { t } = useTranslation();

  const scrollerRef = useRef<HTMLDivElement>(null);
  const stickyBottomRef = useRef(true);
  const [showJumpButton, setShowJumpButton] = useState(false);
  const [bashStatus, setBashStatus] = useState<{ command: string } | { error: string } | null>(
    null,
  );

  const { profileId } = useParams();
  const isDraft = isDraftSessionId(sessionId);

  useEffect(() => {
    open(sessionId);
  }, [sessionId, open]);

  // Turns of this session stream here whichever client started them. A draft
  // has nothing to follow — it does not exist on the daemon yet.
  useEffect(
    () => (isDraft ? undefined : watchTurns(sessionId)),
    [sessionId, isDraft, watchTurns],
  );

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
      <div
        role="status"
        className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground"
      >
        <Loader2 className="size-4 animate-spin" aria-hidden />
        {t('shell.loadingSession')}
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
    lastTurn && !lastTurn.error ? lastTurn : null;
  const rows = chatRows(messages, lastTurnForActive);

  const handleSend = async (
    content: string,
    attachments: ApiAttachment[],
    deliveryMode: 'steer' | 'queue',
  ) => {
    // A draft materializes on this first send: the daemon session is created,
    // the route swaps to its real id, then the message posts normally.
    let sid = sessionId;
    if (isDraft) {
      let created;
      try {
        created = await useSessionsStore.getState().materialize(sessionId);
      } catch (err) {
        toast.error(t('errors.generic'), {
          description: err instanceof Error ? err.message : String(err),
        });
        return;
      }
      sid = created.id;
      const pid = profileId ?? created.profileId;
      if (pid) navigate(profileSessionPath(pid, sid), { replace: true });
    }
    // `/sleep` and `/sleep off` are commands for the daemon, not messages:
    // the daemon starts the good-night turn itself, and every client hears
    // the state on the `sleep` event.
    const sleepCommand = parseSleepCommand(content);
    if (sleepCommand) {
      try {
        if (sleepCommand === 'sleep') await startSleep(sid);
        else await endSleep();
      } catch (err) {
        toast.error(
          t('sleep.failed', { message: err instanceof Error ? err.message : String(err) }),
        );
      }
      return;
    }
    // `!` prefix: bash mode — the daemon runs the command on the user's
    // behalf and returns a masked outcome; the transcript goes to the
    // agent as the next user message, like the TUI's bash mode.
    if (content.startsWith('!')) {
      const command = content.slice(1).trim();
      if (!command) return;
      setBashStatus({ command });
      stickyBottomRef.current = true;
      try {
        const outcome = await runBash(sid, command);
        const transcript = bashTranscript(outcome);
        appendMessage(sid, {
          id: `local-${Date.now()}`,
          role: 'user' as const,
          content: transcript,
          timestamp: new Date().toISOString(),
          attachments: [] as ApiAttachment[],
        });
        if (streaming) {
          await enqueueMessage({ sessionId: sid, content: transcript, attachments: [], deliveryMode });
        } else {
          await send({ sessionId: sid, content: transcript, attachments: [] });
        }
        setBashStatus(null);
      } catch (error) {
        setBashStatus({ error: error instanceof Error ? error.message : String(error) });
      }
      return;
    }
    const userMessage = {
      id: `local-${Date.now()}`,
      role: 'user' as const,
      content,
      timestamp: new Date().toISOString(),
      attachments,
    };
    appendMessage(sid, userMessage);
    // Sending a new message is an explicit user intent to follow the tail.
    stickyBottomRef.current = true;

    if (streaming) {
      await enqueueMessage({
        sessionId: sid,
        content,
        attachments,
        deliveryMode,
      });
      return;
    }

    await send({ sessionId: sid, content, attachments });
  };

  const renderMessage = ({ kind, message: msg }: ChatRow) => {
    if (kind === 'user') {
      return <UserMessage content={msg.content} attachments={msg.attachments} />;
    }
    if (kind === 'turn' && lastTurnForActive) {
      return (
        <AssistantTurn
          turn={lastTurnForActive}
          reasoningEnabled={reasoningEnabled}
          showDecodeSpeed={showDecodeSpeed}
        />
      );
    }
    return <PersistedAssistant message={msg} sessionId={active.id} />;
  };

  return (
    <div className="relative flex h-full flex-col">
      <SessionHeader session={active} />
      <PlanBanner
        sessionId={active.id}
        planState={active.runtime?.planState ?? 'disabled'}
      />
      {isDraftSessionId(active.id) ? null : (
        <YoloToggle sessionId={active.id} yolo={active.runtime?.yolo ?? false} />
      )}
      <BrowserTakeoverBanner session={active} />
      {asleep && (
        <div className="text-muted-foreground flex items-center gap-2 px-4 py-1 text-xs sm:px-6">
          <Moon className="size-3.5" aria-hidden />
          {t('sleep.asleep')}
        </div>
      )}
      <ScrollArea
        ref={scrollerRef}
        onScroll={handleScroll}
        className="flex-1 px-4 pt-6 pb-48 sm:px-6"
        aria-label={active.title || t('common.untitled')}
      >
        <div className="mx-auto w-full max-w-3xl space-y-6">
          {rows.map((row, i) => {
            const msg = row.message;
            const stamp = startsGroup(rows[i - 1]?.message.timestamp, msg.timestamp) ? (
              <p className="pt-2 text-center text-[11px] text-muted-foreground">
                <time dateTime={msg.timestamp}>{formatGroupTime(msg.timestamp, now)}</time>
              </p>
            ) : null;
            return (
              <Fragment key={row.key}>
                {stamp}
                {renderMessage(row)}
              </Fragment>
            );
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
              onRetry={() => void retry(sessionId)}
              onDismiss={() => dismissError(sessionId)}
            />
          )}
        </div>
      </ScrollArea>

      <div
        className={cn(
          'pointer-events-none absolute inset-x-0 bottom-0 flex justify-center px-4 pb-4 sm:px-6 sm:pb-5',
        )}
      >
        <div className="pointer-events-auto mx-auto w-full max-w-3xl space-y-2">
          {showJumpButton && (
            <div className="flex justify-center">
              <Button
                size="sm"
                variant="secondary"
                onClick={scrollToBottom}
                className="gap-1 border border-border bg-background px-2.5 text-muted-foreground shadow-sm hover:bg-accent hover:text-foreground"
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
          {bashStatus && (
            <div
              className={cn(
                'rounded-lg border px-3 py-2 text-xs',
                'command' in bashStatus
                  ? 'border-border bg-muted/40 text-muted-foreground'
                  : 'border-destructive/40 bg-destructive/5 text-destructive',
              )}
            >
              {'command' in bashStatus
                ? t('composer.bashRunning', { command: bashStatus.command })
                : t('composer.bashFailed', { error: bashStatus.error })}
            </div>
          )}
          <Composer
            disabled={!configUsable}
            streaming={streaming}
            onSend={handleSend}
            onCancel={() => void cancel(sessionId)}
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
          <Button size="sm" variant="outline" onClick={onRetry} className="gap-1">
            <RotateCw className="size-3.5" />
            {t('common.retry')}
          </Button>
        )}
        <Button
          size="icon"
          variant="ghost"
          onClick={onDismiss}
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
      className="flex w-full items-start gap-2 rounded-lg border border-border bg-background px-3 py-2.5 text-left text-[13px] transition-colors hover:bg-accent"
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
      <div className="flex-1 min-w-0">
        <div className="font-medium">{t('chat.configGuard.title')}</div>
        <div className="text-xs text-muted-foreground">{t('chat.configGuard.body')}</div>
      </div>
      <span className="shrink-0 text-xs font-medium underline-offset-4 hover:underline">
        {t('chat.configGuard.openSettings')}
      </span>
    </button>
  );
}
