import { useState, useCallback } from 'react';
import { useNavigate } from 'react-router-dom';
import { Bell } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useTranslation } from '@/lib/i18n';
import { useActiveProfile, profileSessionPath } from '@/lib/profile/active';
import {
  getMailbox,
  type NotificationMailboxEntry,
} from '@/lib/api/notifications';
import { useFormatRelativeTime } from '@/lib/utils';
import { cn } from '@/lib/utils';

export function NotificationBell() {
  const { t } = useTranslation();
  const mailbox = useRuntimeStore((s) => s.mailbox);
  const setMailbox = useRuntimeStore.setState;
  const pendingCount = mailbox.filter((m) => m.status === 'pending').length;
  const [open, setOpen] = useState(false);
  const navigate = useNavigate();
  const { profileId } = useActiveProfile();
  const formatRelative = useFormatRelativeTime();

  const handleOpenChange = useCallback(
    async (next: boolean) => {
      setOpen(next);
      if (!next) return;
      try {
        const res = await getMailbox();
        setMailbox({ mailbox: res.items });
      } catch {
        // SSE still feeds mailbox; pull is best-effort
      }
    },
    [setMailbox],
  );

  const handleEntryClick = (entry: NotificationMailboxEntry) => {
    setOpen(false);
    if (!entry.sessionId) return;
    navigate(profileId ? profileSessionPath(profileId, entry.sessionId) : `/s/${entry.sessionId}`);
  };

  const handleOpenSettings = () => {
    setOpen(false);
    navigate('/settings?section=notifications');
  };

  return (
    <Popover open={open} onOpenChange={(v) => void handleOpenChange(v)}>
      <PopoverTrigger asChild>
        <Button
          size="icon"
          variant="ghost"
          aria-label={t('sidebar.notifications.title')}
          className="relative"
        >
          <Bell className="size-4" />
          {pendingCount > 0 && (
            <span
              className="absolute right-1.5 top-1.5 size-1.5 rounded-full bg-brand"
              aria-label={String(pendingCount)}
            />
          )}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 p-0">
        <div className="flex items-center justify-between border-b border-border px-3 py-2">
          <span className="text-sm font-medium">{t('sidebar.notifications.title')}</span>
          <button
            type="button"
            onClick={handleOpenSettings}
            className="text-[11px] text-muted-foreground hover:text-foreground"
          >
            {t('sidebar.notifications.openSettings')}
          </button>
        </div>
        {mailbox.length === 0 ? (
          <div className="px-3 py-6 text-center text-xs text-muted-foreground">
            {t('sidebar.notifications.empty')}
          </div>
        ) : (
          <ScrollArea className="max-h-80">
            <ul className="p-1">
              {mailbox.map((entry) => (
                <li key={entry.id}>
                  <button
                    type="button"
                    onClick={() => handleEntryClick(entry)}
                    className={cn(
                      'flex w-full flex-col items-start gap-0.5 rounded-md px-2.5 py-2 text-left hover:bg-accent',
                      !entry.sessionId && 'cursor-default',
                    )}
                  >
                    <div className="flex w-full items-center gap-2">
                      {entry.status === 'pending' && (
                        <span className="size-1.5 shrink-0 rounded-full bg-brand" />
                      )}
                      <span className="flex-1 truncate text-sm font-medium">
                        {entry.title ?? t('sidebar.notifications.untitled')}
                      </span>
                      <span className="text-[10px] text-muted-foreground">
                        {formatRelative(entry.createdAt)}
                      </span>
                    </div>
                    {entry.body && (
                      <span className="line-clamp-2 text-xs text-muted-foreground">
                        {entry.body}
                      </span>
                    )}
                  </button>
                </li>
              ))}
            </ul>
          </ScrollArea>
        )}
      </PopoverContent>
    </Popover>
  );
}
