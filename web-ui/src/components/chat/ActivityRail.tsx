import { useState } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { ChevronDown, Command, Paperclip, SendToBack, Terminal } from 'lucide-react';
import type { PendingUserMessage, TerminalExecution } from '@/types';
import { ScanningLabel } from './timeline/ScanningLabel';
import { cn } from '@/lib/utils';

interface BackgroundCommandActivity {
  taskId?: string | null;
  outputFile?: string | null;
  outputPreview?: string | null;
  inputNotice?: string | null;
}

interface ActivityRailProps {
  pendingMessages: PendingUserMessage[];
  runningCommands: TerminalExecution[];
  backgroundCommand: BackgroundCommandActivity | null;
  sessionId: string | null;
  onSteerMessage: (index: number) => void;
  onRemoveMessage: (index: number) => void;
}

function pendingDeliveryLabel(message: PendingUserMessage): string {
  return message.deliveryMode === 'steer' ? 'Next' : 'Queued';
}

function pendingMessagePreview(message: PendingUserMessage): string {
  return message.content.trim().length > 0
    ? message.content
    : 'Attachment-only message';
}

function formatDuration(startedAt?: number): string {
  if (!startedAt || !Number.isFinite(startedAt)) {
    return 'now';
  }
  const elapsedSeconds = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
  if (elapsedSeconds < 60) {
    return `${elapsedSeconds}s`;
  }
  const minutes = Math.floor(elapsedSeconds / 60);
  const seconds = elapsedSeconds % 60;
  return `${minutes}m ${seconds}s`;
}

function isSubagentExecution(command: TerminalExecution): boolean {
  const normalizedInput = command.input.trim().toLowerCase();
  return (
    normalizedInput.startsWith('subagent ') ||
    normalizedInput.includes(' ipc subagent ') ||
    normalizedInput.includes('/heel ipc subagent ')
  );
}

function RunningCommandRow({ command }: { command: TerminalExecution }) {
  const [expanded, setExpanded] = useState(false);
  const isSubagent = isSubagentExecution(command);
  return (
    <div className="activity-row group/row">
      <button
        type="button"
        className="flex w-full items-center gap-2.5 px-3 py-2 text-left"
        onClick={() => setExpanded((prev) => !prev)}
      >
        <span className="activity-icon">
          {isSubagent ? (
            <SendToBack className="h-3.5 w-3.5" />
          ) : (
            <Command className="h-3.5 w-3.5" />
          )}
        </span>
        <div className="min-w-0 flex-1">
          <ScanningLabel className="block truncate text-[13px] font-medium tracking-[-0.005em]">
            {command.description}
          </ScanningLabel>
        </div>
        <span className="shrink-0 text-[11px] text-muted-foreground tabular-nums">
          {formatDuration(command.startedAt)}
        </span>
        {command.mode && command.mode !== 'default' && (
          <span className="shrink-0 rounded-full border border-border/60 px-1.5 py-0.5 text-[10px] uppercase tracking-[0.08em] text-muted-foreground">
            {command.mode}
          </span>
        )}
        <motion.span
          animate={{ rotate: expanded ? 180 : 0 }}
          transition={{ duration: 0.12 }}
          className="inline-flex h-4 w-4 shrink-0 items-center justify-center text-muted-foreground"
        >
          <ChevronDown className="h-3.5 w-3.5" />
        </motion.span>
      </button>
      <AnimatePresence initial={false}>
        {expanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: 'auto', opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.12 }}
            className="overflow-hidden"
          >
            <div className="border-t border-border/40 bg-background/40 px-3 py-2">
              <pre className="whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-foreground/80">
                {command.input}
              </pre>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

function BackgroundCommandRow({ command }: { command: BackgroundCommandActivity }) {
  const isWaitingInput = command.inputNotice != null;
  return (
    <div
      className={cn(
        'activity-row',
        isWaitingInput && 'activity-row--attention',
      )}
    >
      <div className="flex items-start gap-2.5 px-3 py-2">
        <span className="activity-icon">
          <Terminal className="h-3.5 w-3.5" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="text-[13px] font-medium tracking-[-0.005em] text-foreground">
            {isWaitingInput
              ? 'Waiting for terminal input'
              : 'Command running in background'}
          </div>
          <div className="mt-0.5 flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
            <span className="rounded-full border border-border/60 px-1.5 py-0.5 text-[10px] uppercase tracking-[0.08em]">
              Background
            </span>
            {command.taskId && (
              <span className="font-mono text-[10px]">
                task {command.taskId}
              </span>
            )}
          </div>
          {isWaitingInput && command.inputNotice ? (
            <div className="mt-2 rounded-xl border border-amber-500/30 bg-amber-500/10 px-2.5 py-1.5 text-[11px] leading-snug text-amber-700 dark:text-amber-200">
              {command.inputNotice}
            </div>
          ) : command.outputPreview ? (
            <div className="mt-2 line-clamp-3 whitespace-pre-wrap font-mono text-[11px] leading-snug text-muted-foreground">
              {command.outputPreview}
            </div>
          ) : command.outputFile ? (
            <div className="mt-2 break-all font-mono text-[10px] text-muted-foreground">
              {command.outputFile}
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function QueuedMessageRow({
  message,
  sessionId,
  index,
  onSteer,
  onRemove,
}: {
  message: PendingUserMessage;
  sessionId: string | null;
  index: number;
  onSteer: () => void;
  onRemove: () => void;
}) {
  return (
    <div
      key={`${sessionId ?? 'session'}-${index}`}
      className="activity-row"
    >
      <div className="flex items-start gap-3 px-3 py-2">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
            <span className="rounded-full border border-border/60 px-1.5 py-0.5 text-[10px] uppercase tracking-[0.08em]">
              {pendingDeliveryLabel(message)}
            </span>
            {message.attachments.length > 0 && (
              <span className="inline-flex items-center gap-1">
                <Paperclip className="h-3 w-3" />
                {message.attachments.length}
              </span>
            )}
          </div>
          <div className="mt-1 line-clamp-2 text-[12px] leading-snug text-foreground/85">
            {pendingMessagePreview(message)}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1.5 pt-0.5">
          <button
            type="button"
            onClick={onSteer}
            className="rounded-full border border-border/60 bg-background/60 px-2 py-0.5 text-[11px] font-medium text-foreground transition-colors hover:border-foreground/40 hover:bg-background"
          >
            Steer
          </button>
          <button
            type="button"
            onClick={onRemove}
            className="rounded-full border border-transparent px-2 py-0.5 text-[11px] font-medium text-muted-foreground transition-colors hover:text-foreground"
          >
            Remove
          </button>
        </div>
      </div>
    </div>
  );
}

export function ActivityRail({
  pendingMessages,
  runningCommands,
  backgroundCommand,
  sessionId,
  onSteerMessage,
  onRemoveMessage,
}: ActivityRailProps) {
  const hasBackgroundCommand =
    backgroundCommand != null &&
    (backgroundCommand.taskId != null ||
      backgroundCommand.inputNotice != null ||
      backgroundCommand.outputPreview != null ||
      backgroundCommand.outputFile != null);

  const hasAnyActivity =
    pendingMessages.length > 0 ||
    runningCommands.length > 0 ||
    hasBackgroundCommand;

  return (
    <AnimatePresence initial={false}>
      {hasAnyActivity && (
        <motion.div
          initial={{ opacity: 0, y: 8 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0, y: 8 }}
          transition={{ duration: 0.15, ease: 'easeOut' }}
          className="activity-rail mb-2"
        >
          {runningCommands.map((command) => (
            <RunningCommandRow key={command.id} command={command} />
          ))}
          {hasBackgroundCommand && backgroundCommand && (
            <BackgroundCommandRow command={backgroundCommand} />
          )}
          {pendingMessages.map((message, index) => (
            <QueuedMessageRow
              key={`${sessionId ?? 'session'}-${index}`}
              message={message}
              sessionId={sessionId}
              index={index}
              onSteer={() => onSteerMessage(index)}
              onRemove={() => onRemoveMessage(index)}
            />
          ))}
        </motion.div>
      )}
    </AnimatePresence>
  );
}
