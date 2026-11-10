import { useEffect, useState } from 'react';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import type { CommandCard, RowChange, RowChangeKind, RowView } from '../../lib/types';

const CHANGE_LABEL_KEY: Record<RowChangeKind, MessageKey> = {
  added: 'tools.card.tasksAdded',
  started: 'tools.card.tasksStarted',
  finished: 'tools.card.tasksFinished',
  blocked: 'tools.card.tasksBlocked',
  unblocked: 'tools.card.tasksUnblocked',
  edited: 'tools.card.tasksEdited',
};

/** `1m 03s` for a span of whole seconds. */
export function elapsedText(seconds: number): string {
  const whole = Math.max(0, Math.floor(seconds));
  return whole >= 60
    ? `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, '0')}s`
    : `${whole}s`;
}

/** The first non-empty line of `text`, marked when more follows. */
export function resultSummary(text: string): string {
  const lines = text.split('\n').filter((line) => line.trim().length > 0);
  const first = lines[0] ?? '';
  return lines.length > 1 ? `${first}…` : first;
}

function rowText(row: RowView): string {
  const detail = row.detail ? ` — ${row.detail.replaceAll('\n', '; ')}` : '';
  return `${row.title} (${row.section})${detail}`;
}

function RowChangeView({ change }: { change: RowChange }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-0.5" data-testid="card-row-change" data-change={change.change}>
      <div className="font-medium text-foreground/90">{t(CHANGE_LABEL_KEY[change.change])}</div>
      {change.before && (
        <div className="whitespace-pre-wrap break-words bg-red-500/10 px-1.5 text-red-700 dark:text-red-400">
          {`- ${rowText(change.before)}`}
        </div>
      )}
      {change.after && (
        <div className="whitespace-pre-wrap break-words bg-emerald-500/10 px-1.5 text-emerald-700 dark:text-emerald-400">
          {`+ ${rowText(change.after)}`}
        </div>
      )}
    </div>
  );
}

/** The current time, ticking each second while `running`. */
function useNow(running: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [running]);
  return now;
}

function SubagentCardView({ card }: { card: Extract<CommandCard, { kind: 'subagent' }> }) {
  const { t } = useTranslation();
  const running = card.state === 'running';
  const now = useNow(running);
  const started = Date.parse(card.startedAt);
  const ended = card.finishedAt ? Date.parse(card.finishedAt) : now;
  const status = running
    ? `${t('tools.card.running')} ${elapsedText((now - started) / 1000)}`
    : `${t('tools.card.done')} ${elapsedText((ended - started) / 1000)}`;
  const result = card.result?.trim() ? card.result : null;
  return (
    <div className="space-y-1" data-testid="card-subagent" data-state={card.state}>
      <div className="flex flex-wrap gap-x-2">
        <span className="font-medium text-foreground/90">{card.agent}</span>
        <span className="text-foreground/80">{card.title}</span>
        <span className="text-muted-foreground">{status}</span>
      </div>
      {result && (
        <details>
          <summary className="cursor-pointer list-none text-muted-foreground">
            {resultSummary(result)}
          </summary>
          <pre
            className="mt-1 max-h-64 overflow-auto whitespace-pre-wrap text-foreground/80 scrollbar-thin"
            aria-label={t('tools.card.fullResult')}
          >
            {result}
          </pre>
        </details>
      )}
    </div>
  );
}

function AskUserCardView({ card }: { card: Extract<CommandCard, { kind: 'askUser' }> }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-1.5" data-testid="card-ask-user">
      {card.questions.map((question, index) => {
        const answer = card.answers?.find((candidate) => candidate.section === question.section);
        return (
          <div key={index} className="space-y-0.5">
            <div className="font-medium text-foreground/90">{question.question}</div>
            {question.options.length > 0 && (
              <ul className="ml-4 list-disc text-muted-foreground">
                {question.options.map((option) => (
                  <li key={option}>{option}</li>
                ))}
              </ul>
            )}
            {answer && (
              <div className="text-emerald-700 dark:text-emerald-400" data-testid="card-answer">
                {`${t('tools.card.answer')}: ${answer.text}`}
              </div>
            )}
          </div>
        );
      })}
      {!card.answers && (
        <div className="text-muted-foreground">{t('tools.card.waiting')}</div>
      )}
    </div>
  );
}

/**
 * A `terminal` call of one of May's commands, rendered from what the daemon
 * recorded the command did instead of its output text.
 */
export function CommandCardView({ card }: { card: CommandCard }) {
  return (
    <div className="space-y-1 text-xs leading-relaxed" data-testid="command-card">
      {card.kind === 'tasks' &&
        card.changes.map((change, index) => <RowChangeView key={index} change={change} />)}
      {card.kind === 'subagent' && <SubagentCardView card={card} />}
      {card.kind === 'askUser' && <AskUserCardView card={card} />}
    </div>
  );
}
