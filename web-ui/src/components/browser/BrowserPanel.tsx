import { useState } from 'react';
import { Loader2, Maximize2, MonitorOff } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import { useBrowserStream } from '@/lib/stores/browserStream';
import { useActiveProfile } from '@/lib/profile/active';
import { startBrowser } from '@/lib/api/browser';
import { BrowserLifecycleStatus } from './BrowserLifecycleStatus';
import { LiveScreen } from './LiveScreen';

/**
 * The inspector's Browser tab: a live thumbnail of the agent's browser
 * that opens the full-screen viewer, or a quiet "asleep" state.
 */
export function BrowserPanel({ sessionId, visible }: { sessionId: string; visible: boolean }) {
  const { t } = useTranslation();
  const browser = useRuntimeStore((s) => s.browser);
  const lifecycle = useRuntimeStore((s) => s.browserLifecycle);
  const openViewer = useBrowserViewStore((s) => s.openViewer);
  const { profile } = useActiveProfile();
  const running = browser?.running ?? false;
  const { stream, state } = useBrowserStream(sessionId, visible && running);
  const [busy, setBusy] = useState(false);
  const agent = profile?.name ?? t('appName');

  if (!running) {
    const wake = async () => {
      setBusy(true);
      try {
        await startBrowser();
      } catch (err) {
        toast.error(t('errors.generic'), { description: err instanceof Error ? err.message : undefined });
      } finally {
        setBusy(false);
      }
    };
    return (
      <div className="flex flex-col items-center justify-center gap-2 px-6 py-16 text-center">
        <MonitorOff className="size-5 text-muted-foreground" strokeWidth={1.5} aria-hidden />
        <p className="text-[13px] text-muted-foreground">{t('browser.asleep')}</p>
        {browser?.enabled ? (
          <Button size="sm" variant="secondary" disabled={busy} onClick={() => void wake()}>
            {busy && <Loader2 className="animate-spin" aria-hidden />}
            {t('browser.wake')}
          </Button>
        ) : (
          <p className="text-xs text-muted-foreground">{t('browser.off')}</p>
        )}
      </div>
    );
  }

  const live = stream !== null && state === 'connected';
  const lost = state === 'failed' || state === 'closed' || state === 'disconnected';

  return (
    <div className="p-4">
      <button
        type="button"
        onClick={openViewer}
        aria-label={t('browser.open')}
        title={t('browser.open')}
        className="group relative block aspect-[16/10] w-full overflow-hidden rounded-lg border border-border bg-muted"
      >
        <LiveScreen stream={stream} className="size-full object-contain" tabIndex={-1} />
        {!live && (
          <span
            role="status"
            className="absolute inset-0 flex items-center justify-center gap-2 text-xs text-muted-foreground"
          >
            {lost ? (
              t('browser.streamLost')
            ) : (
              <>
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
                {t('browser.connecting')}
              </>
            )}
          </span>
        )}
        <span className="absolute right-2 top-2 flex size-7 items-center justify-center rounded-md bg-background/90 text-muted-foreground opacity-0 shadow-sm transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100 touch:opacity-100">
          <Maximize2 className="size-3.5" aria-hidden />
        </span>
      </button>
      <p className="mt-2 text-center text-xs text-muted-foreground">
        {t('browser.screenOf', { name: agent })}
      </p>
      <BrowserLifecycleStatus snapshot={lifecycle} className="mt-0.5 text-center" />
    </div>
  );
}
