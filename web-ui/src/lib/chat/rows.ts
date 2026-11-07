import type { TurnState } from '../stream/reducer';
import type { ApiMessage } from '../types';

/**
 * How one message is drawn. React keeps a component instance across renders
 * while its key and type stay the same, so a row's `key` and `kind` are
 * what decide whether a refresh updates it in place or remounts it.
 */
export interface ChatRow {
  key: string;
  kind: 'user' | 'turn' | 'persisted';
  message: ApiMessage;
}

/**
 * The rows of a session's messages. The turn that just finished is still
 * drawn from its streamed state, under the id of the first assistant message
 * it stored; the turn's other stored messages are part of that drawing.
 */
export function chatRows(messages: ApiMessage[], finishedTurn: TurnState | null): ChatRow[] {
  const rows: ChatRow[] = [];
  for (const message of messages) {
    if (message.role === 'user') {
      rows.push({ key: message.id, kind: 'user', message });
    } else if (finishedTurn && finishedTurn.assistantId === message.id) {
      rows.push({ key: message.id, kind: 'turn', message });
    } else if (!finishedTurn?.messageIds.includes(message.id)) {
      rows.push({ key: message.id, kind: 'persisted', message });
    }
  }
  return rows;
}
