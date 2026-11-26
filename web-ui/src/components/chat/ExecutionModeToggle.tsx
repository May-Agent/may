import { useState } from 'react';
import { toast } from 'sonner';
import { Switch } from '@/components/ui/switch';
import { putSessionExecutionMode } from '@/lib/api/tasks';
import { useTranslation } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';
import type { ExecutionMode } from '@/lib/types';

interface ExecutionModeToggleProps {
  sessionId: string;
  executionMode: ExecutionMode;
}

/** The session's orchestrator execution mode switch. */
export function ExecutionModeToggle({ sessionId, executionMode }: ExecutionModeToggleProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const [busy, setBusy] = useState(false);

  const orchestrator = executionMode.mode === 'orchestrator';

  const change = async (next: boolean) => {
    setBusy(true);
    try {
      const nextMode: ExecutionMode = next ? { mode: 'orchestrator' } : { mode: 'standard' };
      const res = await putSessionExecutionMode(sessionId, nextMode);
      updateRuntime(sessionId, { executionMode: res.runtime.executionMode ?? nextMode });
    } catch (error) {
      toast.error(
        t('executionMode.failed', {
          message: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center gap-2 text-xs text-muted-foreground">
      <span title={t('executionMode.orchestratorDescription')}>{t('executionMode.orchestratorLabel')}</span>
      <Switch
        checked={orchestrator}
        disabled={busy}
        aria-label={t('executionMode.orchestratorDescription')}
        onCheckedChange={(next) => void change(next)}
      />
    </div>
  );
}
