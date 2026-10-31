import { Loader2, MessageSquarePlus } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { SidebarToggle } from '@/components/shell/SidebarToggle';
import { useTranslation } from '@/lib/i18n';
import { useNewChat } from '@/lib/useNewChat';
import { shortcutAria, shortcutLabel } from '@/lib/shortcuts';

export function HomePage() {
  const { t } = useTranslation();
  const { start, creating, enabled } = useNewChat();

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-12 shrink-0 items-center px-2 touch:h-14 sm:px-3">
        <SidebarToggle />
      </header>
      <div className="flex flex-1 items-center justify-center px-4 pb-12 sm:px-6">
        <div className="max-w-md space-y-4 text-center">
          <MessageSquarePlus
            className="mx-auto size-10 text-muted-foreground"
            aria-hidden
            strokeWidth={1.5}
          />
          <h1 className="text-2xl font-semibold tracking-tight">{t('home.welcome')}</h1>
          <p className="text-[15px] text-muted-foreground">{t('home.description')}</p>
          <div className="pt-2">
            <Button
              size="lg"
              onClick={() => void start()}
              disabled={!enabled || creating}
              aria-busy={creating}
              aria-keyshortcuts={shortcutAria('newChat')}
            >
              {creating && <Loader2 className="animate-spin" aria-hidden />}
              {creating ? t('shell.creatingChat') : t('home.newChat')}
            </Button>
            <p className="mt-3 text-xs text-muted-foreground touch:hidden">
              {t('shell.newChatShortcut', { shortcut: shortcutLabel('newChat') })}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
