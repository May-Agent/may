import { useEffect, useState } from 'react';
import { Eye, EyeOff, Save } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import {
  getConfig,
  setFullConfig,
  type UiConfigResponse,
  type UiProviderConfig,
} from '@/lib/api/config';

export function ProvidersSection() {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<UiConfigResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [revealed, setRevealed] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    (async () => {
      setLoading(true);
      try {
        setCfg(await getConfig());
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const updateProvider = (id: string, patch: Partial<UiProviderConfig>) => {
    if (!cfg) return;
    setCfg({
      ...cfg,
      fullConfig: {
        ...cfg.fullConfig,
        providers: cfg.fullConfig.providers.map((p) =>
          p.id === id ? { ...p, ...patch } : p,
        ),
      },
    });
  };

  const toggleReveal = (id: string) => {
    setRevealed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const save = async () => {
    if (!cfg) return;
    setBusy(true);
    try {
      await setFullConfig(cfg.fullConfig);
      toast.success(t('common.save'));
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const providers = cfg?.fullConfig.providers ?? [];

  return (
    <section>
      <SectionHeader
        title={t('settings.providers.title')}
        description={t('settings.providers.description')}
        action={
          <Button disabled={busy || !cfg} onClick={save}>
            <Save className="size-4" />
            {t('common.save')}
          </Button>
        }
      />

      {loading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : providers.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.empty')}</div>
      ) : (
        <ul className="space-y-3">
          {providers.map((p) => {
            const shown = revealed.has(p.id);
            return (
              <li key={p.id} className="rounded-md border border-border p-3">
                <div className="mb-2 flex items-center justify-between">
                  <div>
                    <div className="text-sm font-medium">{p.displayName ?? p.id}</div>
                    <div className="text-xs text-muted-foreground">{p.providerType}</div>
                  </div>
                  <label className="flex cursor-pointer items-center gap-1.5 text-xs">
                    <input
                      type="checkbox"
                      checked={p.enabled}
                      onChange={(e) => updateProvider(p.id, { enabled: e.target.checked })}
                    />
                    {p.enabled ? t('common.enabled') : t('common.disabled')}
                  </label>
                </div>
                <div className="grid gap-2">
                  <div>
                    <label className="text-xs text-muted-foreground">{t('settings.providers.apiKey')}</label>
                    <div className="relative">
                      <input
                        type={shown ? 'text' : 'password'}
                        value={p.apiKey ?? ''}
                        onChange={(e) => updateProvider(p.id, { apiKey: e.target.value })}
                        className="w-full rounded-md border border-border bg-background px-3 py-1.5 pr-9 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
                      />
                      <button
                        type="button"
                        onClick={() => toggleReveal(p.id)}
                        className="absolute inset-y-0 right-1.5 flex items-center text-muted-foreground hover:text-foreground"
                        aria-label={shown ? 'hide' : 'show'}
                      >
                        {shown ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
                      </button>
                    </div>
                  </div>
                  <div>
                    <label className="text-xs text-muted-foreground">{t('settings.providers.baseUrl')}</label>
                    <input
                      value={p.baseUrl ?? ''}
                      onChange={(e) => updateProvider(p.id, { baseUrl: e.target.value })}
                      className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
                    />
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
