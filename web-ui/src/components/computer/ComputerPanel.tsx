import { useEffect, useState } from 'react';
import { Loader2, Maximize2, MonitorOff } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import { useComputerViewStore } from '@/lib/stores/computerView';
import { useComputerStream } from '@/lib/stores/computerStream';
import { useActiveProfile } from '@/lib/profile/active';
import { getComputerStatus, startComputer } from '@/lib/api/computer';
import { LiveScreen } from '../browser/LiveScreen';

/**
 * The inspector's Computer tab: a live thumbnail of the session's virtual
 * desktop that opens the full-screen viewer, or a quiet "asleep" state.
 */
export function ComputerPanel({ sessionId, visible }: { sessionId: string; visible: boolean }) {
  const { t } = useTranslation();
  const openViewer = useComputerViewStore((s) => s.openViewer);
  const { profile } = useActiveProfile();
  const [running, setRunning] = useState<boolean | null>(null);
  const { stream, state } = useComputerStream(sessionId, visible && running === true);
  const [busy, setBusy] = useState(false);
  const agent = profile?.name ?? t('appName');

  useEffect(() => {
    if (!visible) return;
    let alive = true;
    getComputerStatus(sessionId)
      .then((status) => alive && setRunning(status.running))
      .catch(() => alive && setRunning(false));
    return () => {
      alive = false;
    };
  }, [sessionId, visible]);

  if (running !== true) {
    const wake = async () => {
      setBusy(true);
      try {
        await startComputer(sessionId);
        setRunning(true);
      } catch (err) {
        toast.error(t('errors.generic'), { description: err instanceof Error ? err.message : undefined });
      } finally {
        setBusy(false);
      }
    };
    return (
      <div className="flex flex-col items-center justify-center gap-2 px-6 py-16 text-center">
        <MonitorOff className="size-5 text-muted-foreground" strokeWidth={1.5} aria-hidden />
        <p className="text-[13px] text-muted-foreground">{t('computer.asleep')}</p>
        <Button size="sm" variant="secondary" disabled={busy} onClick={() => void wake()}>
          {busy && <Loader2 className="animate-spin" aria-hidden />}
          {t('computer.wake')}
        </Button>
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
        aria-label={t('computer.open')}
        title={t('computer.open')}
        className="group relative block aspect-[16/10] w-full overflow-hidden rounded-lg border border-border bg-muted"
      >
        <LiveScreen stream={stream} className="size-full object-contain" tabIndex={-1} />
        {!live && (
          <span
            role="status"
            className="absolute inset-0 flex items-center justify-center gap-2 text-xs text-muted-foreground"
          >
            {lost ? (
              t('computer.streamLost')
            ) : (
              <>
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
                {t('computer.connecting')}
              </>
            )}
          </span>
        )}
        <span className="absolute right-2 top-2 flex size-7 items-center justify-center rounded-md bg-background/90 text-muted-foreground opacity-0 shadow-sm transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100 touch:opacity-100">
          <Maximize2 className="size-3.5" aria-hidden />
        </span>
      </button>
      <p className="mt-2 text-center text-xs text-muted-foreground">
        {t('computer.screenOf', { name: agent })}
      </p>
    </div>
  );
}
