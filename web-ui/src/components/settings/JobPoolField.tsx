import { useState } from 'react';
import { toast } from 'sonner';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useTranslation } from '@/lib/i18n';
import { useConfigStore } from '@/lib/stores/config';
import { getConfig, setFullConfig } from '@/lib/api/config';
import { parseJobPoolSize } from '@/lib/jobPool';

export function JobPoolField({ disabled }: { disabled?: boolean }) {
  const { t } = useTranslation();
  const saved = useConfigStore((s) => s.config?.fullConfig.sandbox.jobPoolSize ?? null);
  const refresh = useConfigStore((s) => s.refresh);
  // The text being typed; null shows the saved size.
  const [draft, setDraft] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const commit = async () => {
    if (draft === null) return;
    const size = parseJobPoolSize(draft);
    if (size === undefined) {
      toast.error(t('settings.execution.jobPoolInvalid'));
      setDraft(null);
      return;
    }
    if (size === saved || busy) {
      setDraft(null);
      return;
    }
    setBusy(true);
    try {
      const current = await getConfig();
      await setFullConfig({
        ...current.fullConfig,
        sandbox: { ...current.fullConfig.sandbox, jobPoolSize: size },
      });
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setDraft(null);
      setBusy(false);
    }
  };

  return (
    <div className="mt-6">
      <Label htmlFor="job-pool-size">{t('settings.execution.jobPool')}</Label>
      <Input
        id="job-pool-size"
        className="mt-2 max-w-40"
        inputMode="numeric"
        placeholder={t('settings.execution.jobPoolAuto')}
        value={draft ?? (saved === null ? '' : String(saved))}
        disabled={disabled || busy}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={() => void commit()}
        onKeyDown={(event) => {
          if (event.key === 'Enter') event.currentTarget.blur();
        }}
      />
      <p className="mt-3 text-xs text-muted-foreground">
        {t('settings.execution.jobPoolInfo')}
      </p>
    </div>
  );
}
