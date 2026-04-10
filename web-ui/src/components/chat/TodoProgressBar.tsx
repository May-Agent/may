import { cn } from '@/lib/utils';

export interface TodoProgressState {
  items: Array<{ text: string; checked: boolean }>;
  total: number;
  completed: number;
}

function ChecklistIcon() {
  return (
    <svg
      className="h-3.5 w-3.5"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
    >
      <rect x="2" y="2" width="12" height="12" rx="2" />
      <path d="M5 8l2 2 4-4" />
    </svg>
  );
}

export function TodoProgressBar({ progress }: { progress: TodoProgressState }) {
  const percent =
    progress.total > 0 ? (progress.completed / progress.total) * 100 : 0;
  return (
    <div className="shrink-0 px-4 pb-2">
      <div className="max-w-3xl mx-auto">
        <div className="rounded-lg border border-border/60 bg-card/80 px-3 py-2.5 text-sm">
          <div className="flex items-center gap-2 mb-2">
            <div className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
              <ChecklistIcon />
              Progress
            </div>
            <span className="text-xs text-muted-foreground">
              {progress.completed}/{progress.total}
            </span>
            <div className="flex-1 h-1.5 rounded-full bg-muted/50 overflow-hidden">
              <div
                className="h-full rounded-full bg-primary/70 transition-all duration-300"
                style={{ width: `${percent}%` }}
              />
            </div>
          </div>
          <ul className="space-y-0.5">
            {progress.items.map((item, i) => (
              <li
                key={i}
                className="flex items-start gap-1.5 text-xs leading-relaxed"
              >
                <span
                  className={cn(
                    'mt-0.5 shrink-0',
                    item.checked ? 'text-primary' : 'text-muted-foreground/50',
                  )}
                >
                  {item.checked ? '✓' : '○'}
                </span>
                <span
                  className={
                    item.checked
                      ? 'text-muted-foreground line-through'
                      : 'text-foreground'
                  }
                >
                  {item.text}
                </span>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </div>
  );
}
