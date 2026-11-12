import type { TaskRow } from '@/lib/types';
import { openCount } from '@/lib/tasks/rows';
import { useTranslation } from '@/lib/i18n';
import { TaskRowsView } from '@/components/inspector/TaskRowsView';

interface TodoBlockProps {
  rows: TaskRow[];
}

/** The turn's open work: Doing and Queued rows, nothing once all are deleted. */
export function TodoBlock({ rows }: TodoBlockProps) {
  const { t } = useTranslation();
  if (openCount(rows) === 0) return null;
  return (
    <div className="rounded-lg border border-border px-3 py-2">
      <div className="text-xs font-medium">{t('chat.plan')}</div>
      <div className="mt-1.5">
        <TaskRowsView rows={rows} />
      </div>
    </div>
  );
}
