import { useEffect, useMemo, useState } from 'react';
import { toast } from 'sonner';
import { Segmented } from '@/components/ui/segmented';
import { putSessionExecutionMode } from '@/lib/api/tasks';
import { useTranslation } from '@/lib/i18n';
import { useModelsStore } from '@/lib/stores/models';
import { useSessionsStore } from '@/lib/stores/sessions';
import type { ExecutionMode } from '@/lib/types';

interface ExecutionModeToggleProps {
  sessionId: string;
  executionMode: ExecutionMode;
}

type ModeKind = ExecutionMode['mode'];

/** Session execution mode switcher (`standard` | `orchestrator` | `fusion`). */
export function ExecutionModeToggle({ sessionId, executionMode }: ExecutionModeToggleProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const modelGroups = useModelsStore((s) => s.groups);
  const refreshModels = useModelsStore((s) => s.refresh);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (modelGroups === null) {
      void refreshModels();
    }
  }, [modelGroups, refreshModels]);

  const availableModels = useMemo(() => {
    const ids: string[] = [];
    for (const group of modelGroups ?? []) {
      for (const item of group.details) {
        if (!item.outdated) {
          ids.push(item.id);
        }
      }
      if (group.details.length === 0) {
        for (const model of group.models) {
          ids.push(`${group.providerId}::${model}`);
        }
      }
    }
    return ids;
  }, [modelGroups]);

  const currentSidekick = executionMode.mode === 'fusion' ? executionMode.sidekickModel : '';
  const sidekickOptions = useMemo(() => {
    if (currentSidekick && !availableModels.includes(currentSidekick)) {
      return [currentSidekick, ...availableModels];
    }
    return availableModels;
  }, [availableModels, currentSidekick]);

  const applyMode = async (nextMode: ExecutionMode) => {
    setBusy(true);
    try {
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

  const handleModeChange = (next: ModeKind) => {
    if (next === executionMode.mode) return;
    if (next === 'standard') {
      void applyMode({ mode: 'standard' });
      return;
    }
    if (next === 'orchestrator') {
      void applyMode({ mode: 'orchestrator' });
      return;
    }
    const chosen = currentSidekick || availableModels[0];
    if (!chosen) {
      toast.error(t('executionMode.fusionNeedsModel'));
      return;
    }
    void applyMode({ mode: 'fusion', sidekickModel: chosen });
  };

  return (
    <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
      <Segmented<ModeKind>
        size="sm"
        disabled={busy}
        value={executionMode.mode}
        onValueChange={handleModeChange}
        options={[
          { value: 'standard', label: t('executionMode.standardLabel') },
          {
            value: 'orchestrator',
            label: (
              <span title={t('executionMode.orchestratorDescription')}>
                {t('executionMode.orchestratorLabel')}
              </span>
            ),
          },
          {
            value: 'fusion',
            label: (
              <span title={t('executionMode.fusionDescription')}>
                {t('executionMode.fusionLabel')}
              </span>
            ),
          },
        ]}
      />
      {executionMode.mode === 'fusion' ? (
        <select
          aria-label={t('executionMode.sidekickModelLabel')}
          title={t('executionMode.sidekickModelLabel')}
          disabled={busy || sidekickOptions.length === 0}
          value={executionMode.sidekickModel}
          onChange={(e) => {
            const sidekickModel = e.target.value;
            if (sidekickModel) {
              void applyMode({ mode: 'fusion', sidekickModel });
            }
          }}
          className="h-7 max-w-[14rem] rounded-md border border-border bg-background px-2 text-xs text-foreground"
        >
          {sidekickOptions.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
      ) : null}
    </div>
  );
}
