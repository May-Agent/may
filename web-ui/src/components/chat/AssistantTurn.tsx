import { AlertCircle, Zap } from 'lucide-react';
import type { TurnState } from '@/lib/stream/reducer';
import { MessageText } from './MessageText';
import { ReasoningBlock } from './ReasoningBlock';
import { ToolCard } from './ToolCard';
import { TodoBlock } from './TodoBlock';
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
    <div className="space-y-2">
      {turn.reasoning.length > 0 && (
        <ReasoningBlock text={turn.reasoning} streaming={reasoningStreaming} />
      )}
      {turn.segments.map((segment, idx) => {
        if (segment.kind === 'text') {
          const isTail = idx === lastTextIdx;
          return (
            <MessageText
              key={`t-${idx}-${segment.seq}`}
              text={segment.text}
              streaming={streaming && isTail}
              sessionId={turn.sessionId}
            />
          );
        }
        const tool = toolsById.get(segment.toolId);
        if (!tool) return null;
        return <ToolCard key={tool.id} tool={tool} />;
      })}
      {turn.todo && (
        <TodoBlock items={turn.todo.items} total={turn.todo.total} completed={turn.todo.completed} />
      )}
      {showDecodeSpeed && turn.tokensPerSecond != null && (
        <div className="flex items-center gap-1.5 text-xs tabular-nums text-muted-foreground">
          <Zap className="size-3" />
          <span>{t('chat.decodeSpeed', { rate: formatRate(turn.tokensPerSecond) })}</span>
        </div>
      )}
      {streaming && !hasAnySegment && !turn.todo && !turn.reasoning && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-foreground/60 timeline-pulse" />
          {reasoningEnabled ? (
            <span className="shimmer-text">{t('chat.thinking')}…</span>
          ) : (
            <span className="shimmer-text">{t('chat.responding')}…</span>
          )}
        </div>
      )}
      {turn.error && (
        <div className="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
          <AlertCircle className="mt-0.5 size-4 shrink-0" />
          <div>
            <div className="font-medium">{turn.error.code}</div>
            <div className="text-xs opacity-80">{turn.error.message}</div>
          </div>
        </div>
      )}
    </div>
  );
}
