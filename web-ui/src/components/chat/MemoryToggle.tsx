import { useState } from 'react';
import { toast } from 'sonner';
import { Switch } from '@/components/ui/switch';
import { putSessionMemoryOff } from '@/lib/api/tasks';
import { useTranslation } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';

interface MemoryToggleProps {
  sessionId: string;
  memoryOff: boolean;
}

/** Per-session memory switch: turning it off stops reading and saving memories for the rest of the chat. */
export function MemoryToggle({ sessionId, memoryOff }: MemoryToggleProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const [busy, setBusy] = useState(false);

  const turnOff = async (nextEnabled: boolean) => {
    if (nextEnabled || memoryOff) return;
    setBusy(true);
    try {
      const res = await putSessionMemoryOff(sessionId);
      updateRuntime(sessionId, { memoryOff: res.runtime.memoryOff });
    } catch (error) {
      toast.error(
        t('memoryToggle.failed', {
          message: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setBusy(false);
    }
  };

  const title = memoryOff ? t('memoryToggle.offDescription') : t('memoryToggle.description');

  return (
    <div className="flex items-center gap-2 text-xs text-muted-foreground">
      <span title={title}>{t('memoryToggle.label')}</span>
      <Switch
        checked={!memoryOff}
        disabled={busy || memoryOff}
        aria-label={title}
        onCheckedChange={(checked) => void turnOff(checked)}
      />
    </div>
  );
}
