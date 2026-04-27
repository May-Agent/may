import {
  AlertCircle,
  CheckCircle2,
  Compass,
  Download,
  Loader2,
  PauseCircle,
  Power,
  Users,
} from 'lucide-react';
import type { BrowserLifecycleSnapshot } from '@/lib/stores/runtime';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

interface Props {
  snapshot: BrowserLifecycleSnapshot;
  className?: string;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function BrowserLifecycleStatus({ snapshot, className }: Props) {
  const { t } = useTranslation();
  const last = snapshot.last;

  const tone = (
    () => {
      if (!last) return 'idle';
      switch (last.kind) {
        case 'error':
        case 'encoderStalled':
          return 'warn';
        case 'ready':
        case 'pageLoaded':
          return 'ok';
        case 'stopped':
          return 'idle';
        default:
          return 'busy';
      }
    }
  )();

  const Icon = (() => {
    if (!last) return PauseCircle;
    switch (last.kind) {
      case 'chromeFetching':
        return Download;
      case 'starting':
        return Loader2;
      case 'ready':
        return CheckCircle2;
      case 'navigating':
        return Compass;
      case 'pageLoaded':
        return CheckCircle2;
      case 'peerConnected':
      case 'peerDisconnected':
        return Users;
      case 'takeoverChanged':
        return Users;
      case 'encoderStalled':
        return AlertCircle;
      case 'error':
        return AlertCircle;
      case 'stopped':
        return Power;
    }
  })();

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
        return t('browserLive.viewerCount', { count: last.count });
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

  return (
    <div
      className={cn(
        'flex items-center gap-1.5 text-xs',
        tone === 'ok' && 'text-emerald-600 dark:text-emerald-400',
        tone === 'warn' && 'text-amber-600 dark:text-amber-400',
        tone === 'busy' && 'text-muted-foreground',
        tone === 'idle' && 'text-muted-foreground/60',
        className,
      )}
      role="status"
      aria-live="polite"
    >
      <Icon
        className={cn(
          'size-3.5 shrink-0',
          last?.kind === 'starting' && 'animate-spin',
          last?.kind === 'chromeFetching' && 'animate-pulse',
        )}
      />
      <span className="truncate">{label}</span>
    </div>
  );
}
