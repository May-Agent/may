import { useEffect, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { toast } from 'sonner';
import { SectionHeader } from './SectionHeader';
import { Segmented } from '@/components/ui/segmented';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { useConfigStore } from '@/lib/stores/config';
import { getConfig, setFullConfig, type CommandPriority } from '@/lib/api/config';

const OPTIONS: Array<{ value: CommandPriority; labelKey: MessageKey }> = [
  { value: 'normal', labelKey: 'settings.execution.normal' },
  { value: 'low', labelKey: 'settings.execution.low' },
  { value: 'lowest', labelKey: 'settings.execution.lowest' },
];

export function CommandPrioritySection() {
  const { t } = useTranslation();
  const config = useConfigStore((s) => s.config);
  const error = useConfigStore((s) => s.error);
  const refresh = useConfigStore((s) => s.refresh);
  const [busy, setBusy] = useState(false);

  // Seed the store once if nothing has loaded it yet.
  useEffect(() => {
    const { config, loading, refresh } = useConfigStore.getState();
    if (config === null && !loading) {
      void refresh();
    }
  }, []);

  const priority: CommandPriority =
    config?.fullConfig.sandbox.commandPriority ?? 'normal';

  const change = async (next: CommandPriority) => {
    if (next === priority || busy) return;
    setBusy(true);
    try {
      const current = await getConfig();
      await setFullConfig({
        ...current.fullConfig,
        sandbox: { ...current.fullConfig.sandbox, commandPriority: next },
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
      <SectionHeader
        title={t('settings.execution.title')}
        description={t('settings.execution.description')}
        action={busy ? <Loader2 className="size-4 animate-spin" /> : undefined}
      />
      {!config ? (
        <div className="py-10 text-center text-sm text-muted-foreground">
          {error ?? t('common.loading')}
        </div>
      ) : (
        <>
          <Segmented
            value={priority}
            onValueChange={(next) => void change(next)}
            options={OPTIONS.map(({ value, labelKey }) => ({
              value,
              label: t(labelKey),
            }))}
            disabled={busy}
          />
          {error && <p className="mt-3 text-xs text-destructive">{error}</p>}
          <p className="mt-3 text-xs text-muted-foreground">
            {t('settings.execution.info')}
          </p>
        </>
      )}
    </section>
  );
}
