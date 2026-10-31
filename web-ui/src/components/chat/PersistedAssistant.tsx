import type { ApiMessage, ToolSummary, ToolVariant } from '@/lib/types';
import type { TurnToolState } from '@/lib/stream/reducer';
import { MessageText } from './MessageText';
import { ReasoningBlock } from './ReasoningBlock';
import { ToolCard } from './ToolCard';
import { BrowserCard } from './BrowserCard';
import { MessageActions } from './MessageActions';
import { groupBrowserRuns } from '@/lib/browser/activity';

interface PersistedToolEvent {
  stage: 'start' | 'end';
  id: string;
  name: string;
  variant: ToolVariant;
  arguments?: string;
  result?: unknown;
  seq?: number;
  summary?: ToolSummary;
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
      summary: {} as ToolSummary,
      status: 'done' as const,
      seq: evt.seq ?? 0,
    };
    if (evt.stage === 'start') {
      existing.argumentsRaw = evt.arguments ?? existing.argumentsRaw;
      if (evt.summary) existing.summary = evt.summary;
      if (typeof evt.seq === 'number') existing.seq = evt.seq;
    } else {
      existing.argumentsRaw = evt.arguments ?? existing.argumentsRaw;
      if (evt.summary) existing.summary = evt.summary;
      existing.result = evt.result;
      existing.status = 'done';
    }
    map.set(evt.id, existing);
  }
  return Array.from(map.values()).sort((a, b) => a.seq - b.seq);
}

export function PersistedAssistant({
  message,
  sessionId,
}: {
  message: ApiMessage;
  sessionId: string;
}) {
  const tools = extractTools(message.toolEvent);
  const reasoning = typeof message.thinking === 'string' ? message.thinking : '';
  return (
    <div className="group/message space-y-2">
      {reasoning.length > 0 && <ReasoningBlock text={reasoning} />}
      {groupBrowserRuns(tools, (tool) => tool.variant === 'browser').map((group) =>
        group.kind === 'browser' ? (
          <BrowserCard key={group.items[0]!.id} tools={group.items} sessionId={sessionId} />
        ) : (
          <ToolCard key={group.item.id} tool={group.item} />
        ),
      )}
      {message.content.length > 0 && <MessageText text={message.content} sessionId={sessionId} />}
      <MessageActions text={message.content} className="-ml-1" />
    </div>
  );
}
