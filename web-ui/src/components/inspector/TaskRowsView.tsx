import type { TaskReference, TaskRow } from '@/lib/types';
import { openGroups, referenceLabel } from '@/lib/tasks/rows';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

/** A named issue or pull request as plain secondary text. */
export function ReferenceLabel({ reference }: { reference: TaskReference }) {
  return (
    <span className="font-mono text-[11px] text-muted-foreground">
      {referenceLabel(reference)}
    </span>
  );
}

/** Status glyph: a gray ring, dashed while the row is blocked. */
function RowGlyph({ blocked }: { blocked: boolean }) {
  return (
    <span
      className={cn(
        'mt-[3px] size-3.5 shrink-0 rounded-full border-[1.5px]',
        blocked ? 'border-dashed border-foreground/70' : 'border-muted-foreground/70',
      )}
      aria-hidden
    />
  );
}

function TaskRowItem({ row }: { row: TaskRow }) {
  const { t } = useTranslation();
  const blocked = row.blocking !== null;
  return (
    <li className="flex items-start gap-2 py-[3px]">
      <RowGlyph blocked={blocked} />
      <div className="min-w-0 flex-1">
        <span className="whitespace-pre-line text-[13px] leading-5">{row.text}</span>
        {blocked && (
          <div className="text-xs text-foreground/80">
            {t('inspector.blocked', { what: row.blocking ?? '' })}
          </div>
        )}
      </div>
    </li>
  );
}

/** The open work of tasks.md (Doing, Queued), grouped by section; blocked rows are marked. */
export function TaskRowsView({ rows }: { rows: TaskRow[] }) {
  const { t } = useTranslation();
  const headings = { doing: t('inspector.doing'), queued: t('inspector.queued') } as const;
  return (
    <div className="space-y-2.5">
      {openGroups(rows).map((group) => {
        if (group.section === 'roots') return null;
        const heading = headings[group.section];
        return (
          <div key={group.section}>
            <h4 className="mb-0.5 text-xs text-muted-foreground">{heading}</h4>
            <ul aria-label={heading}>
              {group.rows.map((row, i) => (
                <TaskRowItem key={i} row={row} />
              ))}
            </ul>
          </div>
        );
      })}
    </div>
  );
}
