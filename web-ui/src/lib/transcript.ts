import type { ApiAttachment, ApiMessage } from './types';

export interface TranscriptEntry {
  kind: string;
  id: string;
  timestamp?: string | null;
  text?: string;
  content?: string;
  attachments?: ApiAttachment[];
  thinking?: string | null;
  translation?: ApiMessage['translation'];
  stats?: ApiMessage['stats'];
  callId?: string;
  name?: string;
  arguments?: string;
  status?: string;
  result?: unknown;
  interaction?: string;
}

function inferVariant(name?: string): string {
  switch (name) {
    case 'websearch': return 'web_search';
    case 'webfetch': return 'web_fetch';
    case 'terminal': case 'terminal_input': case 'terminal_kill':
    case 'terminal_read': case 'browser': case 'web_search':
    case 'web_fetch': case 'ask': case 'session': case 'task':
      return name!;
    default: return 'generic';
  }
}

export interface TranscriptPage {
  entries: TranscriptEntry[];
  hasMore: boolean;
  cursor: string;
}

/** Fold transcript entries into the ApiMessage render model, mirroring the
 * daemon's old map_db_messages: tool calls fold onto the nearest assistant
 * within a user-bounded turn (trailing tools attach to the last assistant);
 * interactions, hiddenContext, recap and system rows are skipped. */
export function foldTranscript(entries: TranscriptEntry[]): ApiMessage[] {
  const out: ApiMessage[] = [];
  let pendingTools: Record<string, unknown>[] = [];
  let lastAssistant = -1;
  let seq = 0;

  const flush = () => {
    if (pendingTools.length === 0) return;
    if (lastAssistant >= 0) {
      const msg = out[lastAssistant];
      msg.toolEvent = [...(Array.isArray(msg.toolEvent) ? msg.toolEvent : []), ...pendingTools];
    }
    pendingTools = [];
    seq = 0;
  };

  for (const entry of entries) {
    switch (entry.kind) {
      case 'user':
        flush();
        lastAssistant = -1;
        out.push({
          id: `msg-${entry.id}`,
          role: 'user',
          content: entry.text ?? '',
          attachments: entry.attachments ?? [],
          timestamp: entry.timestamp ?? '',
        });
        break;
      case 'assistant': {
        const msg: ApiMessage = {
          id: `msg-${entry.id}`,
          role: 'assistant',
          content: entry.text ?? '',
          thinking: entry.thinking ?? undefined,
          translation: entry.translation ?? undefined,
          stats: entry.stats ?? undefined,
          timestamp: entry.timestamp ?? '',
          toolEvent: pendingTools.length > 0 ? pendingTools : undefined,
        };
        pendingTools = [];
        seq = 0;
        out.push(msg);
        lastAssistant = out.length - 1;
        break;
      }
      case 'toolCall': {
        pendingTools.push({
          stage: 'start',
          id: entry.callId,
          name: entry.name,
          variant: inferVariant(entry.name),
          arguments: entry.arguments,
          seq: seq++,
        });
        if (entry.status !== 'running') {
          pendingTools.push({
            stage: 'end',
            id: entry.callId,
            name: entry.name,
            variant: inferVariant(entry.name),
            result:
              entry.status === 'stopped'
                ? { kind: 'text', text: 'Stopped by the user before it finished.' }
                : entry.result,
            seq: seq++,
          });
        }
        break;
      }
      default:
        break; // interaction*, hiddenContext, recap, system — skipped as before
    }
  }
  flush();
  return out;
}
