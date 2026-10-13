import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SegmentedOption<T extends string> {
  value: T;
  label: ReactNode;
  icon?: ReactNode;
}

export interface SegmentedProps<T extends string> {
  value: T;
  onValueChange: (next: T) => void;
  options: SegmentedOption<T>[];
  className?: string;
  size?: 'sm' | 'md';
  disabled?: boolean;
}

export function Segmented<T extends string>({
  value,
  onValueChange,
  options,
  className,
  size = 'md',
  disabled = false,
}: SegmentedProps<T>) {
  return (
    <div
      role="radiogroup"
      className={cn(
        'inline-flex items-center rounded-lg border border-border bg-card p-0.5',
        className,
      )}
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            disabled={disabled}
            onClick={() => onValueChange(opt.value)}
            className={cn(
              'inline-flex items-center gap-1.5 rounded-md font-medium transition-colors',
              size === 'sm' ? 'px-2.5 py-1 text-xs' : 'px-3 py-1.5 text-sm',
              active
                ? 'bg-background text-foreground shadow-sm'
                : 'text-muted-foreground hover:text-foreground',
            )}
          >
            {opt.icon}
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
