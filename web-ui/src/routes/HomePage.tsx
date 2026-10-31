import { Loader2 } from 'lucide-react';
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
      <header className="flex h-11 shrink-0 items-center px-2 touch:h-14 sm:px-3">
        <SidebarToggle />
      </header>
      <div className="flex flex-1 items-center justify-center px-4 pb-12 sm:px-6">
        <div className="max-w-sm space-y-2 text-center">
          <h1 className="text-lg font-semibold">{t('home.welcome')}</h1>
          <p className="text-[13px] text-muted-foreground">{t('home.description')}</p>
          <div className="pt-3">
            <Button
              onClick={() => void start()}
              disabled={!enabled || creating}
              aria-busy={creating}
              aria-keyshortcuts={shortcutAria('newChat')}
            >
              {creating && <Loader2 className="animate-spin" aria-hidden />}
              {creating ? t('shell.creatingChat') : t('home.newChat')}
            </Button>
            <p className="mt-2 text-xs text-muted-foreground touch:hidden">
              {t('shell.newChatShortcut', { shortcut: shortcutLabel('newChat') })}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
