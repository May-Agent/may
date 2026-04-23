import { useEffect, useState } from 'react';
import { Play, Square } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { getBrowserStatus, startBrowser, stopBrowser } from '@/lib/api/browser';
import type { BrowserStatusPayload } from '@/lib/types';
import { cn } from '@/lib/utils';

export function BrowserSection() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<BrowserStatusPayload | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const refresh = async () => {
    setLoading(true);
    try {
      setStatus(await getBrowserStatus());
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    refresh();
  }, []);

  const toggle = async () => {
    if (!status) return;
    setBusy(true);
    try {
      const next = status.running ? await stopBrowser() : await startBrowser();
      setStatus(next);
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
      <SectionHeader title={t('settings.browser.title')} description={t('settings.browser.description')} />

      {loading || !status ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : (
        <div className="rounded-md border border-border p-4">
          <div className="mb-3 flex items-center gap-2">
            <span
              className={cn(
                'inline-block size-2 rounded-full',
                status.running ? 'bg-emerald-500' : 'bg-muted-foreground/40',
              )}
            />
            <span className="text-sm font-medium">
              {t('settings.browser.status')}: {status.running ? t('common.running') : t('common.disabled')}
            </span>
          </div>
          {status.cdpEndpoint && (
            <pre className="mb-3 rounded bg-muted/50 px-2 py-1 text-xs font-mono break-all">
              {status.cdpEndpoint}
            </pre>
          )}
          <Button disabled={busy} onClick={toggle}>
            {status.running ? (
              <>
                <Square className="size-4" />
                {t('settings.browser.stop')}
              </>
            ) : (
              <>
                <Play className="size-4" />
                {t('settings.browser.start')}
              </>
            )}
          </Button>
        </div>
      )}
    </section>
  );
}
