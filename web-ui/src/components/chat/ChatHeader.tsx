import type { RefObject } from 'react';
import { FileText, PanelLeft, Pencil, Share2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import type { SessionPlanState } from '@/types';

interface ChatHeaderProps {
  isSidebarOpen: boolean;
  onToggleSidebar: () => void;
  isNewChat: boolean;
  shouldShowWelcome: boolean;
  sessionTitle: string;
  isEditingTitle: boolean;
  titleInput: string;
  titleInputRef: RefObject<HTMLInputElement | null>;
  onTitleInputChange: (value: string) => void;
  onBeginEdit: () => void;
  onTitleSubmit: () => void;
  onTitleCancel: () => void;
  planState: SessionPlanState;
  onPlanStateToggle: () => void;
}

function planButtonLabel(state: SessionPlanState): string {
  if (state === 'disabled') return 'Plan';
  if (state === 'draft') return 'Approve';
  return 'Reopen';
}

function planButtonTitle(state: SessionPlanState): string {
  if (state === 'disabled') return 'Enter plan mode';
  if (state === 'draft') return 'Approve plan';
  return 'Reopen planning';
}

export function ChatHeader({
  isSidebarOpen,
  onToggleSidebar,
  isNewChat,
  shouldShowWelcome,
  sessionTitle,
  isEditingTitle,
  titleInput,
  titleInputRef,
  onTitleInputChange,
  onBeginEdit,
  onTitleSubmit,
  onTitleCancel,
  planState,
  onPlanStateToggle,
}: ChatHeaderProps) {
  return (
    <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
      <div className="flex-1 flex justify-start">
        <Button
          variant="ghost"
          size="icon"
          onClick={onToggleSidebar}
          className={
            isNewChat
              ? 'text-muted-foreground hover:text-foreground'
              : 'text-muted-foreground hover:text-foreground hidden md:flex'
          }
          title={isSidebarOpen ? 'Close sidebar' : 'Open sidebar'}
        >
          <PanelLeft className="h-5 w-5" />
        </Button>
      </div>
      <div className="flex items-center gap-2 min-w-0">
        {!shouldShowWelcome &&
          (isEditingTitle ? (
            <Input
              ref={titleInputRef}
              value={titleInput}
              onChange={(e) => onTitleInputChange(e.target.value)}
              onBlur={onTitleSubmit}
              onKeyDown={(e) => {
                if (e.key === 'Enter') onTitleSubmit();
                if (e.key === 'Escape') onTitleCancel();
              }}
              className="h-auto py-0 w-[300px] text-sm font-medium text-foreground text-center bg-transparent border-none focus-visible:ring-0 px-0"
            />
          ) : (
            <div
              className="flex items-center gap-2 group cursor-pointer min-w-0"
              onClick={onBeginEdit}
            >
              <span className="text-sm font-medium text-foreground truncate max-w-[300px]">
                {sessionTitle}
              </span>
              <Pencil className="h-3 w-3 text-muted-foreground opacity-0 group-hover:opacity-100 transition-opacity" />
            </div>
          ))}
      </div>
      <div className="flex-1 flex justify-end gap-1">
        {!shouldShowWelcome && (
          <Button
            variant="ghost"
            size="sm"
            className="h-8 px-2 text-xs text-muted-foreground hover:text-foreground"
            onClick={onPlanStateToggle}
            title={planButtonTitle(planState)}
          >
            <FileText className="mr-1.5 h-4 w-4" />
            {planButtonLabel(planState)}
          </Button>
        )}
        {!shouldShowWelcome && (
          <Button
            variant="ghost"
            size="icon"
            className="text-muted-foreground hover:text-foreground"
            title="Share chat"
          >
            <Share2 className="h-5 w-5" />
          </Button>
        )}
      </div>
    </div>
  );
}
