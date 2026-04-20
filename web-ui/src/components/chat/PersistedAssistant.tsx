import type { ApiMessage } from '@/lib/types';
import type { TurnToolState } from '@/lib/stream/reducer';
import type { ToolVariant } from '@/lib/types';
import { MessageText } from './MessageText';
import { ReasoningBlock } from './ReasoningBlock';
import { ToolCard } from './ToolCard';

interface PersistedToolEvent {
  stage: 'start' | 'end';
  id: string;
  name: string;
  variant: ToolVariant;
  arguments?: string;
  result?: unknown;
  seq?: number;
}

function extractTools(toolEvent: unknown): TurnToolState[] {
  if (!Array.isArray(toolEvent)) return [];
  const map = new Map<string, TurnToolState>();
  for (const raw of toolEvent as unknown[]) {
    if (!raw || typeof raw !== 'object') continue;
    const evt = raw as PersistedToolEvent;
    const existing = map.get(evt.id) ?? {
      id: evt.id,
      name: evt.name,
      variant: evt.variant,
      argumentsRaw: '',
      status: 'done' as const,
      seq: 0,
    };
    if (evt.stage === 'start') {
      existing.argumentsRaw = evt.arguments ?? existing.argumentsRaw;
    } else {
      existing.argumentsRaw = evt.arguments ?? existing.argumentsRaw;
      existing.result = evt.result;
      existing.status = 'done';
    }
    map.set(evt.id, existing);
  }
  return Array.from(map.values());
}

export function PersistedAssistant({ message }: { message: ApiMessage }) {
  const tools = extractTools(message.toolEvent);
  const reasoning = typeof message.thinking === 'string' ? message.thinking : '';
  return (
    <div className="space-y-2">
      {reasoning.length > 0 && <ReasoningBlock text={reasoning} />}
      {tools.map((tool) => (
        <ToolCard key={tool.id} tool={tool} />
      ))}
      {message.content.length > 0 && <MessageText text={message.content} />}
    </div>
  );
}
