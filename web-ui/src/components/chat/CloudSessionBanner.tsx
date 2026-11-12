import { Loader2, Moon } from 'lucide-react';
import { type MessageKey, useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useSessionsStore } from '@/lib/stores/sessions';
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

function Waiting({ message }: { message: MessageKey }) {
  const { t } = useTranslation();
  return (
    <div
      className="flex items-center gap-2 border-b border-border px-4 py-1.5 text-[13px] text-muted-foreground sm:px-6"
      role="status"
    >
      <Loader2 className="size-3.5 animate-spin" aria-hidden />
      {t(message)}
    </div>
  );
}

/**
 * A cloud session's codespace: what it is doing while it comes up, with a
 * spinner, or that it is suspended until a message wakes it. `creating`
 * holds while a draft's codespace is being created, before the session has
 * an id the daemon's steps name.
 */
export function CloudSessionBanner({
  sessionId,
  creating = false,
}: {
  sessionId: string;
  creating?: boolean;
}) {
  const { t } = useTranslation();
  const step = useRuntimeStore((s) => s.cloudSessions[sessionId]);
  const suspended = useSessionsStore((s) => s.suspended[sessionId] === true);

  if (creating) return <Waiting message="cloudSession.creating" />;

  if (step && step.phase !== 'ready') {
    if (step.phase === 'failed') {
      return (
        <div className="border-b border-border px-4 py-1.5 text-[13px] text-destructive sm:px-6">
          {t('cloudSession.failed', { error: step.error ?? '' })}
        </div>
      );
    }
    return <Waiting message={STEP[step.phase]} />;
  }

  if (suspended) {
    return (
      <div
        className="flex items-center gap-2 border-b border-border px-4 py-1.5 text-[13px] text-muted-foreground sm:px-6"
        role="status"
      >
        <Moon className="size-3.5" aria-hidden />
        {t('cloudSession.suspended')}
      </div>
    );
  }

  return null;
}
