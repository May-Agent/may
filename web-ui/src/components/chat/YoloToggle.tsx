import { useState } from 'react';
import { toast } from 'sonner';
import { Switch } from '@/components/ui/switch';
import { putSessionYolo } from '@/lib/api/tasks';
import { useTranslation } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';

interface YoloToggleProps {
  sessionId: string;
  yolo: boolean;
}

/** The session's yolo switch: while on, every approval request is granted at once. */
export function YoloToggle({ sessionId, yolo }: YoloToggleProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const [busy, setBusy] = useState(false);

  const change = async (next: boolean) => {
    setBusy(true);
    try {
      const res = await putSessionYolo(sessionId, next);
      updateRuntime(sessionId, { yolo: res.yolo });
    } catch (error) {
      toast.error(
        t('yolo.failed', { message: error instanceof Error ? error.message : String(error) }),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center justify-end gap-2 px-4 py-1 text-xs text-muted-foreground sm:px-6">
      <span title={t('yolo.description')}>{t('yolo.label')}</span>
      <Switch
        checked={yolo}
        disabled={busy}
        aria-label={t('yolo.description')}
        onCheckedChange={(next) => void change(next)}
      />
    </div>
  );
}
