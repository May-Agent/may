import { useEffect } from 'react';
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
} from 'lucide-react';
import { useUiStore } from '@/lib/stores/ui';
import { useSessionsStore } from '@/lib/stores/sessions';
import { LOCALES, useTranslation, type Locale } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useActiveProfile, profileSessionPath } from '@/lib/profile/active';

export function CommandPalette() {
  const open = useUiStore((s) => s.commandOpen);
  const setOpen = useUiStore((s) => s.setCommandOpen);
  const setTheme = useUiStore((s) => s.setTheme);
  const order = useSessionsStore((s) => s.order);
  const index = useSessionsStore((s) => s.index);
  const createNew = useSessionsStore((s) => s.createNew);
  const navigate = useNavigate();
  const { t, setLocale, locale } = useTranslation();
  const { profileId } = useActiveProfile();
  const sessionLink = (id: string) => (profileId ? profileSessionPath(profileId, id) : `/s/${id}`);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setOpen(!useUiStore.getState().commandOpen);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [setOpen]);

  const run = (fn: () => void | Promise<void>) => {
    void Promise.resolve(fn()).finally(() => setOpen(false));
  };

  return (
    <DialogPrimitive.Root open={open} onOpenChange={setOpen}>
      <DialogPortal>
        <DialogPrimitive.Overlay
          className={cn(
            'fixed inset-0 z-50 bg-background/50 backdrop-blur-md',
            'data-[state=open]:animate-in data-[state=closed]:animate-out',
            'data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0',
          )}
        />
        <DialogPrimitive.Content
          className={cn(
            'fixed left-1/2 top-1/2 z-50 w-full max-w-xl -translate-x-1/2 -translate-y-1/2',
            'rounded-xl border border-border/60 bg-card/95 backdrop-blur-xl shadow-2xl',
            'duration-200 ease-out',
            'data-[state=open]:animate-in data-[state=closed]:animate-out',
            'data-[state=open]:fade-in-0 data-[state=closed]:fade-out-0',
            'data-[state=open]:zoom-in-95 data-[state=closed]:zoom-out-95',
          )}
        >
          <DialogPrimitive.Title className="sr-only">{t('common.search')}</DialogPrimitive.Title>
          <Command className="flex flex-col overflow-hidden rounded-lg" loop>
            <Command.Input
              placeholder={t('commandPalette.placeholder')}
              className="w-full border-b border-border bg-transparent px-4 py-3 text-sm outline-none placeholder:text-muted-foreground"
            />
            <Command.List className="max-h-[400px] overflow-y-auto p-2 scrollbar-thin">
              <Command.Empty className="px-3 py-6 text-center text-sm text-muted-foreground">
                {t('common.empty')}
              </Command.Empty>

              <Command.Group heading={t('commandPalette.groups.actions')} className="text-xs text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2">
                <PaletteItem
                  icon={<Plus className="size-4" />}
                  label={t('commandPalette.actions.newChat')}
                  onSelect={() =>
                    run(async () => {
                      if (!profileId) return;
                      const s = await createNew(profileId);
                      navigate(profileSessionPath(profileId, s.id));
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
      className="flex cursor-pointer items-center gap-2 rounded-md px-3 py-2 text-sm aria-selected:bg-accent"
    >
      <span className="text-muted-foreground">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {trailing}
    </Command.Item>
  );
}
