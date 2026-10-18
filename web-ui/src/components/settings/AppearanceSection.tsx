import { useEffect, useState } from 'react';
import { Moon, Sun, Laptop } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { useUiStore, type Theme } from '@/lib/stores/ui';
import { useConfigStore } from '@/lib/stores/config';
import { getConfig, setFullConfig } from '@/lib/api/config';
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
  const config = useConfigStore((s) => s.config);
  const refresh = useConfigStore((s) => s.refresh);
  const [busy, setBusy] = useState(false);
  const { t } = useTranslation();

  // Seed the store once if nothing has loaded it yet.
  useEffect(() => {
    const { config, loading, refresh } = useConfigStore.getState();
    if (config === null && !loading) {
      void refresh();
    }
  }, []);

  const showDecodeSpeed = config?.fullConfig.preferences.showDecodeSpeed ?? true;

  const toggleDecodeSpeed = async (next: boolean) => {
    if (next === showDecodeSpeed || busy) return;
    setBusy(true);
    try {
      const current = await getConfig();
      await setFullConfig({
        ...current.fullConfig,
        preferences: { ...current.fullConfig.preferences, showDecodeSpeed: next },
      });
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

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
      {config && (
        <div className="mt-6 flex items-start justify-between gap-4">
          <div>
            <div className="text-sm font-medium">{t('settings.decodeSpeed.title')}</div>
            <div className="mt-0.5 text-xs text-muted-foreground">
              {t('settings.decodeSpeed.description')}
            </div>
          </div>
          <Switch
            checked={showDecodeSpeed}
            onCheckedChange={(next) => void toggleDecodeSpeed(next)}
            disabled={busy}
            aria-label={t('settings.decodeSpeed.title')}
          />
        </div>
      )}
    </section>
  );
}
