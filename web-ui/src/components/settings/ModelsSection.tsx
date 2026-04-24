import { useEffect, useState } from 'react';
import { Check, Star } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { listModelsByProvider, type ProviderModelsGroup } from '@/lib/api/models';
import { getConfig } from '@/lib/api/config';
import { setLastModel } from '@/lib/api/profiles';
import { cn } from '@/lib/utils';

export function ModelsSection() {
  const { t } = useTranslation();
  const [groups, setGroups] = useState<ProviderModelsGroup[]>([]);
  const [current, setCurrent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const refresh = async () => {
    setLoading(true);
    try {
      const [res, cfg] = await Promise.all([listModelsByProvider(), getConfig()]);
      setGroups(res.providers);
      setCurrent(cfg.fullConfig.lastModel ?? null);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    refresh();
  }, []);

  const markDefault = async (model: string) => {
    setBusy(true);
    try {
      await setLastModel(model);
      setCurrent(model);
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
      <SectionHeader title={t('settings.models.title')} description={t('settings.models.description')} />

      {loading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : groups.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.empty')}</div>
      ) : (
        <div className="space-y-6">
          {groups.map((g) => (
            <div key={g.providerId}>
              <div className="mb-1.5 text-xs font-medium uppercase tracking-wider text-muted-foreground">
                {g.providerId}
              </div>
              <ul className="space-y-1">
                {g.details.map((d) => {
                  const active = d.id === current;
                  return (
                    <li
                      key={d.id}
                      className={cn(
                        'flex items-center justify-between rounded-md border border-border px-3 py-2 text-sm',
                        active && 'border-primary bg-primary/5',
                      )}
                    >
                      <div className="min-w-0">
                        <div className="flex items-center gap-1.5 truncate font-mono text-xs">
                          {d.modelId}
                          {active && <Check className="size-3.5 text-primary" />}
                        </div>
                        <div className="truncate text-xs text-muted-foreground">
                          {d.nickname ?? d.tier ?? ''}
                          {d.contextWindow ? ` · ${d.contextWindow.toLocaleString()} ctx` : ''}
                        </div>
                      </div>
                      {!active && (
                        <Button
                          size="sm"
                          variant="outline"
                          disabled={busy}
                          onClick={() => markDefault(d.id)}
                        >
                          <Star className="size-3.5" />
                          {t('settings.models.setDefault')}
                        </Button>
                      )}
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
