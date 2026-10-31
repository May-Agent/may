import { CheckCircle2, Circle, Hourglass } from 'lucide-react';
import type { IssueRef, TaskNode, TaskTree } from '@/lib/tasks/taskTree';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

/** A linked issue: a link when the document gave a URL, a label otherwise. */
export function IssueChip({ issue }: { issue: IssueRef }) {
  const className =
    'inline-flex h-5 items-center rounded-full border border-border bg-background px-1.5 font-mono text-[11px] text-muted-foreground';
  if (issue.url) {
    return (
      <a
        href={issue.url}
        target="_blank"
        rel="noreferrer"
        className={cn(className, 'text-primary hover:underline')}
      >
        {issue.label}
      </a>
    );
  }
  return <span className={className}>{issue.label}</span>;
}

function TaskItem({ node }: { node: TaskNode }) {
  const { t } = useTranslation();
  return (
    <li>
      <div className="flex items-start gap-2 py-1">
        {node.checked ? (
          <CheckCircle2 className="mt-px size-4 shrink-0 text-success" aria-hidden />
        ) : node.blockedBy ? (
          <Hourglass className="mt-px size-4 shrink-0 text-warning" aria-hidden />
        ) : (
          <Circle className="mt-px size-4 shrink-0 text-muted-foreground" aria-hidden />
        )}
        <div className="min-w-0 flex-1">
          <span className="sr-only">{node.checked ? t('inspector.taskDone') : t('inspector.taskOpen')}: </span>
          <span
            className={cn(
              'text-[13px] leading-5',
              node.checked && 'text-muted-foreground line-through decoration-muted-foreground/50',
            )}
          >
            {node.text}
          </span>
          {(node.issues.length > 0 || node.blockedBy) && (
            <div className="mt-1 flex flex-wrap items-center gap-1">
              {node.blockedBy && (
                <span className="text-[11px] font-medium text-warning">{t('inspector.blockedOn')}</span>
              )}
              {node.issues.map((issue) => (
                <IssueChip key={issue.label} issue={issue} />
              ))}
            </div>
          )}
        </div>
      </div>
      {node.children.length > 0 && (
        <ul className="ml-2 border-l border-border pl-3">
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
    <div className="space-y-3">
      {tree.groups.map((group, gi) => (
        <div key={gi}>
          {group.heading && (
            <h4 className="mb-0.5 text-xs font-semibold text-muted-foreground">{group.heading}</h4>
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
