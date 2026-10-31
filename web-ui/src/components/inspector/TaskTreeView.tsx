import { Check } from 'lucide-react';
import type { IssueRef, TaskNode, TaskTree } from '@/lib/tasks/taskTree';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

/** A linked issue as plain secondary text; a link when the document gave a URL. */
export function IssueLabel({ issue }: { issue: IssueRef }) {
  const className = 'font-mono text-[11px] text-muted-foreground';
  if (issue.url) {
    return (
      <a
        href={issue.url}
        target="_blank"
        rel="noreferrer"
        className={cn(className, 'underline-offset-2 hover:text-foreground hover:underline')}
      >
        {issue.label}
      </a>
    );
  }
  return <span className={className}>{issue.label}</span>;
}

/** Status glyph: a gray ring, filled with a check once done. */
function TaskGlyph({ checked, blocked }: { checked: boolean; blocked: boolean }) {
  if (checked) {
    return (
      <span className="mt-[3px] flex size-3.5 shrink-0 items-center justify-center rounded-full bg-muted-foreground text-background">
        <Check className="size-2.5" strokeWidth={3} aria-hidden />
      </span>
    );
  }
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

function TaskItem({ node }: { node: TaskNode }) {
  const { t } = useTranslation();
  const blocked = Boolean(node.blockedBy);
  return (
    <li>
      <div className="flex items-start gap-2 py-[3px]">
        <TaskGlyph checked={node.checked} blocked={blocked} />
        <div className="min-w-0 flex-1">
          <span className="sr-only">{node.checked ? t('inspector.taskDone') : t('inspector.taskOpen')}: </span>
          <span className={cn('text-[13px] leading-5', node.checked && 'text-muted-foreground')}>
            {node.text}
          </span>
          {(node.issues.length > 0 || blocked) && (
            <div className="flex flex-wrap items-baseline gap-x-2 text-xs">
              {blocked && <span className="text-foreground/80">{t('inspector.blockedOn')}</span>}
              {node.issues.map((issue) => (
                <IssueLabel key={issue.label} issue={issue} />
              ))}
            </div>
          )}
        </div>
      </div>
      {node.children.length > 0 && (
        <ul className="ml-[6px] border-l border-border pl-3">
          {node.children.map((child, i) => (
            <TaskItem key={i} node={child} />
          ))}
        </ul>
      )}
    </li>
  );
}

/** The checkbox tree of tasks.md, grouped under its headings. */
export function TaskTreeView({ tree }: { tree: TaskTree }) {
  return (
    <div className="space-y-2.5">
      {tree.groups.map((group, gi) => (
        <div key={gi}>
          {group.heading && (
            <h4 className="mb-0.5 text-xs text-muted-foreground">{group.heading}</h4>
          )}
          <ul aria-label={group.heading ?? undefined}>
            {group.tasks.map((node, i) => (
              <TaskItem key={i} node={node} />
            ))}
          </ul>
        </div>
      ))}
    </div>
  );
}
