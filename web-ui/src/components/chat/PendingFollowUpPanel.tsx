import { Clock3, MoreHorizontal, Paperclip } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { PendingUserMessage } from "@/types";

const PENDING_FOLLOW_UP_PANEL_MAX_WIDTH_CLASS = "max-w-[440px]";

interface PendingFollowUpPanelProps {
  messages: PendingUserMessage[];
  title: string;
  subtitle: string;
  sessionId: string | null;
  onSteerMessage: (index: number) => void;
  onRemoveMessage: (index: number) => void;
}

function pendingDeliveryLabel(message: PendingUserMessage): string {
  return message.deliveryMode === "steer" ? "Next" : "Later";
}

function pendingMessagePreview(message: PendingUserMessage): string {
  return message.content.trim().length > 0
    ? message.content
    : "Attachment-only message";
}

export function PendingFollowUpPanel({
  messages,
  title,
  subtitle,
  sessionId,
  onSteerMessage,
  onRemoveMessage,
}: PendingFollowUpPanelProps) {
  if (messages.length === 0) {
    return null;
  }

  return (
    <div className="flex justify-end px-3 pb-2 pt-2">
      <div className={`pending-follow-up-shell w-full rounded-[18px] border ${PENDING_FOLLOW_UP_PANEL_MAX_WIDTH_CLASS}`}>
        <div className="pending-follow-up-header flex items-center justify-between border-b px-3.5 py-2.5">
          <div className="flex min-w-0 items-center gap-2.5">
            <span className="pending-follow-up-header-icon inline-flex h-7 w-7 items-center justify-center rounded-lg text-zinc-200">
              <Clock3 className="h-3.5 w-3.5" />
            </span>
            <div className="min-w-0">
              <div className="pending-follow-up-header-title text-[11px] text-zinc-100">
                {title}
              </div>
              <div className="pending-follow-up-header-subtitle truncate text-[11px]">
                {subtitle}
              </div>
            </div>
          </div>
          <div className="pending-follow-up-count-pill text-[11px] font-medium tabular-nums text-zinc-300">
            {messages.length}
          </div>
        </div>
        <div className="pending-follow-up-list p-2">
          {messages.map((message, index) => (
            <div
              key={`${sessionId ?? "session"}-${index}`}
              className="pending-follow-up-item group grid grid-cols-[auto,1fr,auto] items-start gap-3 rounded-[14px] px-3 py-3"
            >
              <div className="pending-follow-up-order mt-0.5 inline-flex h-7 min-w-7 items-center justify-center rounded-lg text-[10px] font-medium tabular-nums text-zinc-300">
                {index + 1}
              </div>
              <div className="min-w-0 flex-1 space-y-1.5">
                <div className="pending-follow-up-message-content line-clamp-2 text-[12.5px] leading-[1.45] text-zinc-100/95">
                  {pendingMessagePreview(message)}
                </div>
                <div className="pending-follow-up-meta flex flex-wrap items-center gap-2 text-[10px] leading-none">
                  <span className="pending-follow-up-state-pill inline-flex items-center rounded-full px-2 py-1 text-zinc-200">
                    {pendingDeliveryLabel(message)}
                  </span>
                  {message.attachments.length > 0 && (
                    <span className="inline-flex items-center gap-1.5 text-zinc-400">
                      <Paperclip className="h-3 w-3" />
                      {message.attachments.length} attachment
                      {message.attachments.length > 1 ? "s" : ""}
                    </span>
                  )}
                </div>
              </div>
              <div className="flex shrink-0 items-start gap-1.5">
                <button
                  type="button"
                  onClick={() => onSteerMessage(index)}
                  className="pending-follow-up-steer h-7 rounded-full border px-3 text-[11px] font-medium text-zinc-200 transition-colors"
                >
                  Steer
                </button>

                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <button
                      type="button"
                      className="pending-follow-up-more inline-flex h-7 w-7 items-center justify-center rounded-full border text-zinc-500 transition-colors hover:text-zinc-100"
                      aria-label="More actions"
                    >
                      <MoreHorizontal className="h-4 w-4" />
                    </button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" className="w-44">
                    <DropdownMenuItem onClick={() => onSteerMessage(index)}>
                      Steer to composer
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      className="text-destructive focus:text-destructive"
                      onClick={() => onRemoveMessage(index)}
                    >
                      Remove
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
