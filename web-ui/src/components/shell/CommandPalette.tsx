import { useNavigate } from 'react-router-dom';
import { Command } from 'cmdk';
import { DialogPortal } from '@/components/ui/dialog';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import {
  MessageSquare,
  Plus,
  Settings as SettingsIcon,
  Sun,
  Moon,
  Monitor,
  Languages,
  PanelLeft,
  PanelRight,
} from 'lucide-react';
import { useNewChat } from '@/lib/useNewChat';
import { shortcutLabel } from '@/lib/shortcuts';
import { useUiStore } from '@/lib/stores/ui';
import { useSessionsStore } from '@/lib/stores/sessions';
import { LOCALES, useTranslation, type Locale } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useActiveProfile, profileSessionPath } from '@/lib/profile/active';
import { INSPECTOR_COLUMN_QUERY, SIDEBAR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';

export function CommandPalette() {
  const open = useUiStore((s) => s.commandOpen);
  const setOpen = useUiStore((s) => s.setCommandOpen);
  const setTheme = useUiStore((s) => s.setTheme);
  const order = useSessionsStore((s) => s.order);
  const index = useSessionsStore((s) => s.index);
  const { start: startNewChat } = useNewChat();
  const sidebarCollapsed = useUiStore((s) => s.sidebarCollapsed);
  const inspectorOpen = useUiStore((s) => s.inspectorOpen);
  const sidebarColumn = useMediaQuery(SIDEBAR_COLUMN_QUERY);
  const inspectorColumn = useMediaQuery(INSPECTOR_COLUMN_QUERY);
  const navigate = useNavigate();
  const { t, setLocale, locale } = useTranslation();
  const { profileId } = useActiveProfile();
  const sessionLink = (id: string) => (profileId ? profileSessionPath(profileId, id) : `/s/${id}`);

  const run = (fn: () => void | Promise<void>) => {
    void Promise.resolve(fn()).finally(() => setOpen(false));
  };

  return (
    <DialogPrimitive.Root open={open} onOpenChange={setOpen}>
      <DialogPortal>
        <DialogPrimitive.Overlay
          className={cn(
            'fixed inset-0 z-50 bg-black/20 dark:bg-black/40',
            'data-[state=open]:animate-in data-[state=closed]:animate-out',
            'data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0',
          )}
        />
        <DialogPrimitive.Content
          className={cn(
            'fixed left-1/2 top-[12vh] z-50 w-[calc(100%-2rem)] max-w-xl -translate-x-1/2',
            'rounded-lg border border-border bg-popover shadow-2xl',
            'duration-200 ease-out',
            'data-[state=open]:animate-in data-[state=closed]:animate-out',
            'data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0',
            'data-[state=open]:zoom-in-95 data-[state=closed]:zoom-out-95',
          )}
          aria-describedby={undefined}
        >
          <DialogPrimitive.Title className="sr-only">{t('common.search')}</DialogPrimitive.Title>
          <Command className="flex flex-col overflow-hidden rounded-lg" loop>
            <Command.Input
              placeholder={t('commandPalette.placeholder')}
              aria-label={t('commandPalette.placeholder')}
              className="h-12 w-full border-b border-border bg-transparent px-4 text-[15px] outline-none placeholder:text-muted-foreground touch:text-base"
            />
            <Command.List className="max-h-[400px] overflow-y-auto p-2 scrollbar-thin">
              <Command.Empty className="px-3 py-6 text-center text-sm text-muted-foreground">
                {t('common.empty')}
              </Command.Empty>

              <Command.Group heading={t('commandPalette.groups.actions')} className="text-xs text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2">
                <PaletteItem
                  icon={<Plus className="size-4" />}
                  label={t('commandPalette.actions.newChat')}
                  trailing={<Kbd>{shortcutLabel('newChat')}</Kbd>}
                  onSelect={() => run(startNewChat)}
                />
                <PaletteItem
                  icon={<PanelLeft className="size-4" />}
                  label={
                    sidebarColumn && !sidebarCollapsed ? t('shell.hideSidebar') : t('shell.showSidebar')
                  }
                  value="toggle-sidebar"
                  trailing={<Kbd>{shortcutLabel('toggleSidebar')}</Kbd>}
                  onSelect={() =>
                    run(() => {
                      const ui = useUiStore.getState();
                      if (sidebarColumn) ui.toggleSidebar();
                      else ui.setSidebarSheetOpen(true);
                    })
                  }
                />
                <PaletteItem
                  icon={<PanelRight className="size-4" />}
                  label={
                    inspectorColumn && inspectorOpen ? t('shell.hideInspector') : t('shell.showInspector')
                  }
                  value="toggle-inspector"
                  trailing={<Kbd>{shortcutLabel('toggleInspector')}</Kbd>}
                  onSelect={() =>
                    run(() => {
                      const ui = useUiStore.getState();
                      if (inspectorColumn) ui.toggleInspector();
                      else ui.setInspectorSheetOpen(true);
                    })
                  }
                />
                <PaletteItem
                  icon={<SettingsIcon className="size-4" />}
                  label={t('commandPalette.actions.goSettings')}
                  onSelect={() => run(() => navigate('/settings'))}
                />
              </Command.Group>

              <Command.Group heading={t('commandPalette.groups.theme')} className="text-xs text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2">
                <PaletteItem
                  icon={<Sun className="size-4" />}
                  label={t('settings.theme.light')}
                  onSelect={() => run(() => setTheme('light'))}
                />
                <PaletteItem
                  icon={<Moon className="size-4" />}
                  label={t('settings.theme.dark')}
                  onSelect={() => run(() => setTheme('dark'))}
                />
                <PaletteItem
                  icon={<Monitor className="size-4" />}
                  label={t('settings.theme.system')}
                  onSelect={() => run(() => setTheme('system'))}
                />
              </Command.Group>

              <Command.Group heading={t('commandPalette.groups.language')} className="text-xs text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2">
                {LOCALES.map((l) => (
                  <PaletteItem
                    key={l.id}
                    icon={<Languages className="size-4" />}
                    label={l.nativeLabel}
                    value={`lang-${l.id}-${l.label}`}
                    trailing={l.id === locale ? <span className="text-xs text-muted-foreground">✓</span> : null}
                    onSelect={() => run(() => setLocale(l.id as Locale))}
                  />
                ))}
              </Command.Group>

              {order.length > 0 && (
                <Command.Group heading={t('commandPalette.groups.sessions')} className="text-xs text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2">
                  {order.map((id) => {
                    const entry = index[id];
                    if (!entry) return null;
                    return (
                      <PaletteItem
                        key={id}
                        icon={<MessageSquare className="size-4" />}
                        label={entry.title || t('common.untitled')}
                        value={`session-${id}-${entry.title}`}
                        onSelect={() => run(() => navigate(sessionLink(id)))}
                      />
                    );
                  })}
                </Command.Group>
              )}
            </Command.List>
          </Command>
        </DialogPrimitive.Content>
      </DialogPortal>
    </DialogPrimitive.Root>
  );
}

interface PaletteItemProps {
  icon: React.ReactNode;
  label: string;
  value?: string;
  trailing?: React.ReactNode;
  onSelect: () => void;
}

function PaletteItem({ icon, label, value, trailing, onSelect }: PaletteItemProps) {
  return (
    <Command.Item
      value={value ?? label}
      onSelect={onSelect}
      className="flex min-h-9 cursor-pointer items-center gap-3 rounded-md px-3 py-2 text-sm aria-selected:bg-accent touch:min-h-11"
    >
      <span className="text-muted-foreground">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {trailing}
    </Command.Item>
  );
}

function Kbd({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="font-sans text-xs tracking-wide text-muted-foreground">{children}</kbd>
  );
}
