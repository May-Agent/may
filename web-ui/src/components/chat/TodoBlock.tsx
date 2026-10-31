import { CheckSquare, Square } from 'lucide-react';
import type { SseTodoItem } from '@/lib/types';
import { useTranslation } from '@/lib/i18n';

interface TodoBlockProps {
  items: SseTodoItem[];
  total: number;
  completed: number;
}

export function TodoBlock({ items, total, completed }: TodoBlockProps) {
  const { t } = useTranslation();
  if (items.length === 0) return null;
  return (
    <div className="rounded-lg border border-border px-3 py-2">
      <div className="flex items-center justify-between text-xs">
        <span className="font-medium">{t('chat.plan')}</span>
        <span className="text-muted-foreground">
          {completed}/{total}
        </span>
      </div>
      <ul className="mt-1.5 space-y-1">
        {items.map((item, i) => (
          <li key={i} className="flex items-start gap-2 text-[13px]">
            {item.checked ? (
              <CheckSquare className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
            ) : (
              <Square className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
            )}
            <span className={item.checked ? 'text-muted-foreground' : ''}>
              {item.text}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}
