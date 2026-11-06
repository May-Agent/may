import { useEffect, useRef } from 'react';
import { toast } from 'sonner';
import { useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';

/** Tells the user when the sleep state changes and when the health reminder is due. */
export function SleepNotices() {
  const { t } = useTranslation();
  const asleep = useRuntimeStore((s) => s.sleep?.asleep ?? null);
  const reminders = useRuntimeStore((s) => s.sleepReminders);
  const lastAsleep = useRef<boolean | null>(null);
  const lastReminders = useRef(reminders);

  useEffect(() => {
    // The first report is the baseline: attaching mid-sleep shows no notice.
    if (asleep !== null && lastAsleep.current !== null && asleep !== lastAsleep.current) {
      toast(asleep ? t('sleep.asleep') : t('sleep.awake'));
    }
    lastAsleep.current = asleep;
  }, [asleep, t]);

  useEffect(() => {
    if (reminders > lastReminders.current) toast(t('sleep.reminder'));
    lastReminders.current = reminders;
  }, [reminders, t]);

  return null;
}
