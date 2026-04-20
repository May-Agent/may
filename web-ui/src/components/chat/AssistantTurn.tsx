import { AlertCircle } from 'lucide-react';
import type { TurnState } from '@/lib/stream/reducer';
import { MessageText } from './MessageText';
import { ReasoningBlock } from './ReasoningBlock';
import { ToolCard } from './ToolCard';
import { TodoBlock } from './TodoBlock';

interface AssistantTurnProps {
  turn: TurnState;
}

export function AssistantTurn({ turn }: AssistantTurnProps) {
  const streaming = turn.status === 'streaming';

  return (
    <div className="space-y-2">
      {turn.reasoning.length > 0 && (
        <ReasoningBlock text={turn.reasoning} streaming={streaming && turn.text.length === 0} />
      )}
      {turn.tools.map((tool) => (
        <ToolCard key={tool.id} tool={tool} />
      ))}
      {turn.todo && (
        <TodoBlock items={turn.todo.items} total={turn.todo.total} completed={turn.todo.completed} />
      )}
      {turn.text.length > 0 && <MessageText text={turn.text} streaming={streaming} />}
      {streaming && turn.text.length === 0 && turn.tools.length === 0 && !turn.todo && !turn.reasoning && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span className="inline-block w-1.5 h-1.5 rounded-full bg-foreground/60 animate-pulse" />
          <span className="shimmer-text">Thinking</span>
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
