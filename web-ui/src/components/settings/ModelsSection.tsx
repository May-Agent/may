import { useEffect, useState } from 'react';
import { Check, Star } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { getConfig, setFullConfig, withCompactionModel } from '@/lib/api/config';
import { setLastModel } from '@/lib/api/profiles';
import { useModelsStore } from '@/lib/stores/models';
import { cn } from '@/lib/utils';

export function ModelsSection() {
  const { t } = useTranslation();
  const groups = useModelsStore((s) => s.groups) ?? [];
  const refreshModels = useModelsStore((s) => s.refresh);
  const modelsLoading = useModelsStore((s) => s.loading);
  const fetchedAt = useModelsStore((s) => s.fetchedAt);
  const [current, setCurrent] = useState<string | null>(null);
  const [compaction, setCompaction] = useState<string | null>(null);
  const [currentLoaded, setCurrentLoaded] = useState(false);
  const [busy, setBusy] = useState(false);

  // The models list is SWR-cached and shared across the app; only the
  // "default model" pointer is local-page state that we still fetch here.
  useEffect(() => {
    void refreshModels();
    void (async () => {
      try {
        const cfg = await getConfig();
        setCurrent(cfg.fullConfig.lastModel ?? null);
        setCompaction(cfg.fullConfig.models.compactionModel ?? null);
      } finally {
        setCurrentLoaded(true);
      }
    })();
  }, [refreshModels]);

  const loading = !currentLoaded || (modelsLoading && fetchedAt === null);

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

  // Selecting the compaction model again returns compaction to the session model.
  const toggleCompaction = async (model: string) => {
    const next = model === compaction ? null : model;
    setBusy(true);
    try {
      const cfg = await getConfig();
      await setFullConfig(withCompactionModel(cfg.fullConfig, next));
      setCompaction(next);
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
                  const forCompaction = d.id === compaction;
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
                      <div className="flex gap-1.5">
                        <Button
                          size="sm"
                          variant={forCompaction ? 'default' : 'outline'}
                          disabled={busy}
                          onClick={() => toggleCompaction(d.id)}
                        >
                          {t('settings.models.useForCompaction')}
                        </Button>
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
                      </div>
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
