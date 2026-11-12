import type { BrowserLifecycleSnapshot } from '@/lib/stores/runtime';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { formatBytes } from '@/lib/formatBytes';

interface Props {
  snapshot: BrowserLifecycleSnapshot;
  className?: string;
}

/** The browser's latest lifecycle step as one line of secondary text. */
export function BrowserLifecycleStatus({ snapshot, className }: Props) {
  const { t } = useTranslation();
  const last = snapshot.last;

  const label = (() => {
    if (!last) return t('browserLive.lifecycle.idle');
    switch (last.kind) {
      case 'chromeFetching':
        return last.totalBytes
          ? `${t('browserLive.lifecycle.fetchingChrome')} ${formatBytes(last.progressBytes)} / ${formatBytes(last.totalBytes)}`
          : `${t('browserLive.lifecycle.fetchingChrome')} ${formatBytes(last.progressBytes)}`;
      case 'starting':
        return t('browserLive.lifecycle.starting');
      case 'ready':
        return t('browserLive.lifecycle.ready');
      case 'navigating':
        return `${t('browserLive.lifecycle.navigating')} ${last.url}`;
      case 'pageLoaded':
        return last.title ?? last.url;
      case 'peerConnected':
      case 'peerDisconnected':
        return snapshot.pageTitle ?? snapshot.pageUrl ?? t('browserLive.lifecycle.ready');
      case 'takeoverChanged':
        return last.sessionId
          ? t('browserLive.lifecycle.takenOver')
          : t('browserLive.lifecycle.released');
      case 'encoderStalled':
        return t('browserLive.lifecycle.stalled');
      case 'error':
        return last.message;
      case 'stopped':
        return t('browserLive.lifecycle.stopped');
    }
  })();

  const failed = last?.kind === 'error' || last?.kind === 'encoderStalled';

  return (
    <p
      className={cn('truncate text-xs', failed ? 'text-destructive' : 'text-muted-foreground', className)}
      role="status"
      aria-live="polite"
      title={label}
    >
      {label}
    </p>
  );
}
