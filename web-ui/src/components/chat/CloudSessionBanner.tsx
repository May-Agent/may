import { Loader2 } from 'lucide-react';
import { type MessageKey, useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import type { CloudSessionPhase } from '@/lib/types';

/** The waiting phases' messages; `ready` shows nothing. */
const STEP: Record<Exclude<CloudSessionPhase, 'ready' | 'failed'>, MessageKey> = {
  creating: 'cloudSession.creating',
  starting: 'cloudSession.starting',
  checking: 'cloudSession.checking',
  downloading: 'cloudSession.downloading',
  installing: 'cloudSession.installing',
  cloning: 'cloudSession.cloning',
  starting_daemon: 'cloudSession.startingDaemon',
  connecting: 'cloudSession.connecting',
};

/** While a cloud session's codespace comes up: what it is doing, with a spinner. */
export function CloudSessionBanner({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const step = useRuntimeStore((s) => s.cloudSessions[sessionId]);
  if (!step || step.phase === 'ready') return null;

  if (step.phase === 'failed') {
    return (
      <div className="border-b border-border px-4 py-1.5 text-[13px] text-destructive sm:px-6">
        {t('cloudSession.failed', { error: step.error ?? '' })}
      </div>
    );
  }

  return (
    <div
      className="flex items-center gap-2 border-b border-border px-4 py-1.5 text-[13px] text-muted-foreground sm:px-6"
      role="status"
    >
      <Loader2 className="size-3.5 animate-spin" aria-hidden />
      {t(STEP[step.phase])}
    </div>
  );
}
