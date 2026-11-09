import { AlertCircle } from 'lucide-react';
import type { TurnState } from '@/lib/stream/reducer';
import { TranslatedText } from './TranslatedText';
import { ReasoningBlock } from './ReasoningBlock';
import { ToolCard } from './ToolCard';
import { BrowserCard } from './BrowserCard';
import { MessageActions } from './MessageActions';
import { groupBrowserRuns } from '@/lib/browser/activity';
import { TodoBlock } from './TodoBlock';
import { openCount } from '@/lib/tasks/rows';
import { useTranslation } from '@/lib/i18n';

interface AssistantTurnProps {
  turn: TurnState;
  reasoningEnabled?: boolean;
  showDecodeSpeed?: boolean;
}

function formatRate(rate: number): string {
  return rate >= 100 ? Math.round(rate).toString() : rate.toFixed(1);
}

export function AssistantTurn({
  turn,
  reasoningEnabled = false,
  showDecodeSpeed = true,
}: AssistantTurnProps) {
  const streaming = turn.status === 'streaming';
  const { t } = useTranslation();

  const toolsById = new Map(turn.tools.map((tool) => [tool.id, tool] as const));
  const lastTextIdx = (() => {
    for (let i = turn.segments.length - 1; i >= 0; i -= 1) {
      if (turn.segments[i]!.kind === 'text') return i;
    }
    return -1;
  })();

  const hasAnySegment = turn.segments.length > 0;
  const reasoningStreaming = streaming && !hasAnySegment;

  return (
    <div className="group/message space-y-2">
      {(turn.reasoning.length > 0 || turn.reasoningMs != null) && (
        <ReasoningBlock
          text={turn.reasoning}
          seconds={turn.reasoningMs != null ? turn.reasoningMs / 1000 : undefined}
          streaming={reasoningStreaming}
        />
      )}
      {groupBrowserRuns(
        turn.segments.map((segment, idx) => ({ segment, idx })),
        ({ segment }) =>
          segment.kind === 'tool' && toolsById.get(segment.toolId)?.variant === 'browser',
      ).map((group) => {
        if (group.kind === 'browser') {
          const tools = group.items.flatMap(({ segment }) => {
            const tool = segment.kind === 'tool' ? toolsById.get(segment.toolId) : undefined;
            return tool ? [tool] : [];
          });
          return <BrowserCard key={`c-${group.items[0]!.idx}`} tools={tools} sessionId={turn.sessionId} />;
        }
        const { segment, idx } = group.item;
        if (segment.kind === 'text') {
          const isTail = idx === lastTextIdx;
          return (
            <TranslatedText
              key={`t-${idx}-${segment.seq}`}
              text={segment.text}
              translation={segment.translation}
              streaming={streaming && isTail}
              sessionId={turn.sessionId}
            />
          );
        }
        const tool = toolsById.get(segment.toolId);
        if (!tool) return null;
        return <ToolCard key={tool.id} tool={tool} />;
      })}
      {turn.todo && <TodoBlock rows={turn.todo.rows} />}
      {!streaming && (
        <MessageActions
          className="-ml-1"
          text={turn.segments.flatMap((s) => (s.kind === 'text' ? [s.text] : [])).join('\n\n')}
        />
      )}
      {showDecodeSpeed && turn.tokensPerSecond != null && (
        <div className="flex items-center gap-1.5 text-xs tabular-nums text-muted-foreground">
          <span>{t('chat.decodeSpeed', { rate: formatRate(turn.tokensPerSecond) })}</span>
        </div>
      )}
      {streaming && turn.hold && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span className="shimmer-text">
            {turn.hold.reason === 'quota'
              ? t('chat.quotaHold', { at: new Date(turn.hold.until).toLocaleString() })
              : turn.hold.reason === 'retry'
                ? t('chat.retryHold', {
                    n: String(turn.hold.retry),
                    at: new Date(turn.hold.until).toLocaleTimeString(),
                  })
                : t('chat.networkHold')}
          </span>
        </div>
      )}
      {streaming && !hasAnySegment && !(turn.todo && openCount(turn.todo.rows) > 0) && !turn.reasoning && !turn.hold && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          {reasoningEnabled ? (
            <span className="shimmer-text">{t('chat.thinking')}…</span>
          ) : (
            <span className="shimmer-text">{t('chat.responding')}…</span>
          )}
        </div>
      )}
      {turn.error && (
        <div
          className={
            turn.error.code === 'repo_declined'
              ? 'flex items-start gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2 text-sm text-muted-foreground'
              : 'flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive'
          }
        >
          <AlertCircle className="mt-0.5 size-4 shrink-0" />
          <div>
            {turn.error.code !== 'repo_declined' && (
              <div className="font-medium">{turn.error.code}</div>
            )}
            <div className="text-xs opacity-80">{turn.error.message}</div>
          </div>
        </div>
      )}
    </div>
  );
}
