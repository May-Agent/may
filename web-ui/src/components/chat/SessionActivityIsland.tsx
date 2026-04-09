import { Command, Paperclip, SendToBack } from "lucide-react";
import type { PendingUserMessage, TerminalExecution } from "@/types";

const ACTIVITY_ISLAND_MAX_WIDTH_CLASS = "max-w-[360px]";

interface BackgroundCommandActivity {
  taskId?: string | null;
  outputFile?: string | null;
  outputPreview?: string | null;
  inputNotice?: string | null;
}

interface SessionActivityIslandProps {
  pendingMessages: PendingUserMessage[];
  runningCommands: TerminalExecution[];
  backgroundCommand: BackgroundCommandActivity | null;
  sessionId: string | null;
  onSteerMessage: (index: number) => void;
  onRemoveMessage: (index: number) => void;
}

function pendingDeliveryLabel(message: PendingUserMessage): string {
  return message.deliveryMode === "steer" ? "Next" : "Queued";
}

function pendingMessagePreview(message: PendingUserMessage): string {
  return message.content.trim().length > 0
    ? message.content
    : "Attachment-only message";
}

function formatDuration(startedAt?: number): string {
  if (!startedAt || !Number.isFinite(startedAt)) {
    return "now";
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
    normalizedInput.startsWith("subagent ") ||
    normalizedInput.includes(" ipc subagent ") ||
    normalizedInput.includes("/heel ipc subagent ")
  );
}

export function SessionActivityIsland({
  pendingMessages,
  runningCommands,
  backgroundCommand,
  sessionId,
  onSteerMessage,
  onRemoveMessage,
}: SessionActivityIslandProps) {
  const hasBackgroundCommand =
    backgroundCommand?.taskId != null || backgroundCommand?.inputNotice != null;
  if (
    pendingMessages.length === 0 &&
    runningCommands.length === 0 &&
    !hasBackgroundCommand
  ) {
    return null;
  }
  const queuedOnly = pendingMessages.length > 0 && runningCommands.length === 0 && !hasBackgroundCommand;

  return (
    <div className="flex justify-end">
      <div
        className={`session-activity-island w-full ${queuedOnly ? "max-w-[320px]" : ACTIVITY_ISLAND_MAX_WIDTH_CLASS}`}
      >
        <div className="flex flex-col">
          {runningCommands.length > 0 && (
            runningCommands.map((command) => (
              <div
                key={command.id}
                className="session-activity-item session-activity-item--running grid grid-cols-[auto,1fr,auto] items-start gap-2.5"
              >
                <span className="session-activity-icon">
                  {isSubagentExecution(command) ? (
                    <SendToBack className="h-3.5 w-3.5" />
                  ) : (
                    <Command className="h-3.5 w-3.5" />
                  )}
                </span>
                <div className="min-w-0">
                  <div className="line-clamp-2 text-[12px] font-medium leading-[1.35] text-foreground">
                    {command.description}
                  </div>
                  <div className="mt-0.5 flex flex-wrap items-center gap-1.5 text-[10px] text-muted-foreground">
                    <span className="session-activity-meta">
                      {isSubagentExecution(command) ? "Subagent" : "Command"}
                    </span>
                    {command.mode && (
                      <span className="session-activity-meta">{command.mode}</span>
                    )}
                  </div>
                </div>
                <div className="timeline-running-label pt-0.5 text-[10px] tabular-nums">
                  {formatDuration(command.startedAt)}
                </div>
              </div>
            ))
          )}

          {hasBackgroundCommand && backgroundCommand && (
            <div className="session-activity-item grid grid-cols-[auto,1fr] items-start gap-2.5">
              <span className="session-activity-icon">
                <SendToBack className="h-3.5 w-3.5" />
              </span>
              <div className="min-w-0">
                <div className="text-[12px] font-medium leading-[1.35] text-foreground">
                  {backgroundCommand.inputNotice
                    ? "Waiting for terminal input"
                    : "Command moved to the background"}
                </div>
                <div className="mt-0.5 flex flex-wrap items-center gap-1.5 text-[10px] text-muted-foreground">
                  <span className="session-activity-meta">Background</span>
                  {backgroundCommand.taskId && (
                    <span className="session-activity-meta">
                      Task {backgroundCommand.taskId}
                    </span>
                  )}
                </div>
                {backgroundCommand.inputNotice ? (
                  <div className="mt-1.5 rounded-xl border border-amber-500/30 bg-amber-500/10 px-2 py-1.5 text-[11px] leading-[1.4] text-amber-700 dark:text-amber-200">
                    {backgroundCommand.inputNotice}
                  </div>
                ) : backgroundCommand.outputPreview ? (
                  <div className="mt-1.5 line-clamp-3 text-[11px] leading-[1.4] text-muted-foreground">
                    {backgroundCommand.outputPreview}
                  </div>
                ) : backgroundCommand.outputFile ? (
                  <div className="mt-1.5 text-[10px] text-muted-foreground">
                    Output: {backgroundCommand.outputFile}
                  </div>
                ) : null}
              </div>
            </div>
          )}

          {pendingMessages.length > 0 && (
            pendingMessages.map((message, index) => (
              <div
                key={`${sessionId ?? "session"}-${index}`}
                className="session-activity-item session-activity-item--queued grid grid-cols-[1fr,auto] items-start gap-3"
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-1.5 text-[10px] text-muted-foreground">
                    <span className="session-activity-meta">
                      {pendingDeliveryLabel(message)}
                    </span>
                    {message.attachments.length > 0 && (
                      <span className="inline-flex items-center gap-1 text-muted-foreground">
                        <Paperclip className="h-3 w-3" />
                        {message.attachments.length}
                      </span>
                    )}
                  </div>
                  <div className="mt-1 line-clamp-2 text-[11px] leading-[1.4] text-foreground/85">
                    {pendingMessagePreview(message)}
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1.5 pt-0.5">
                  <button
                    type="button"
                    onClick={() => onSteerMessage(index)}
                    className="session-activity-action"
                  >
                    Steer
                  </button>
                  <button
                    type="button"
                    onClick={() => onRemoveMessage(index)}
                    className="session-activity-action text-muted-foreground hover:text-foreground"
                  >
                    Remove
                  </button>
                </div>
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
