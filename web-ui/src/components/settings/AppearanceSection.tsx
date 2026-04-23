import { Moon, Sun, Laptop } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useUiStore, type Theme } from '@/lib/stores/ui';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { SectionHeader } from './SectionHeader';

const THEME_OPTIONS: Array<{ value: Theme; labelKey: MessageKey; icon: typeof Sun }> = [
  { value: 'light', labelKey: 'settings.theme.light', icon: Sun },
  { value: 'dark', labelKey: 'settings.theme.dark', icon: Moon },
  { value: 'system', labelKey: 'settings.theme.system', icon: Laptop },
];

export function AppearanceSection() {
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);
  const { t } = useTranslation();

  return (
    <section>
      <SectionHeader title={t('settings.theme.title')} />
      <div className="flex gap-2">
        {THEME_OPTIONS.map(({ value, labelKey, icon: Icon }) => (
          <Button
            key={value}
            variant={theme === value ? 'default' : 'outline'}
            onClick={() => setTheme(value)}
          >
            <Icon className="size-4" />
            {t(labelKey)}
          </Button>
        ))}
      </div>
    </section>
  );
}
