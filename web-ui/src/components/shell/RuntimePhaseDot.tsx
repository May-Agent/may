import { cn } from '@/lib/utils';
import type { SessionRuntimeState } from '@/lib/types';
import { useTranslation } from '@/lib/i18n';

const COLORS: Record<SessionRuntimeState, string> = {
  idle: 'bg-transparent',
  running: 'bg-emerald-500 animate-pulse',
  waiting_permission: 'bg-amber-500',
  waiting_workspace: 'bg-amber-500',
  waiting_ask_user: 'bg-sky-500',
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
    <span className="inline-flex items-center gap-1.5 rounded-full border border-border bg-card px-2 py-0.5 text-[11px] text-muted-foreground">
      <span className={cn('inline-block size-1.5 rounded-full', COLORS[state])} />
      {t(LABEL_KEYS[state] as 'runtime.running')}
    </span>
  );
}
