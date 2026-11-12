import { cn } from '@/lib/utils';
import type { SessionRuntimeState } from '@/lib/types';
import { useTranslation } from '@/lib/i18n';

// A small gray dot: filled while working, hollow while waiting on the user.
const COLORS: Record<SessionRuntimeState, string> = {
  idle: 'bg-transparent',
  running: 'bg-muted-foreground motion-safe:animate-pulse',
  waiting_permission: 'border border-foreground',
  waiting_workspace: 'border border-muted-foreground',
  waiting_ask_user: 'border border-foreground',
};

const LABEL_KEYS: Record<SessionRuntimeState, string> = {
  idle: 'runtime.idle',
  running: 'runtime.running',
  waiting_permission: 'runtime.waitingPermission',
  waiting_workspace: 'runtime.waitingWorkspace',
  waiting_ask_user: 'runtime.waitingAskUser',
};

export function RuntimePhaseDot({ state }: { state: SessionRuntimeState | undefined }) {
  const { t } = useTranslation();
  if (!state || state === 'idle') return null;
  return (
    <span
      title={t(LABEL_KEYS[state] as 'runtime.running')}
      className={cn('inline-block size-1.5 shrink-0 rounded-full', COLORS[state])}
    />
  );
}

export function RuntimePhaseBadge({ state }: { state: SessionRuntimeState | undefined }) {
  const { t } = useTranslation();
  if (!state || state === 'idle') return null;
  return (
    <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
      <span className={cn('inline-block size-1.5 rounded-full', COLORS[state])} />
      {t(LABEL_KEYS[state] as 'runtime.running')}
    </span>
  );
}
