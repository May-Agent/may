import type { HTMLAttributes } from 'react';
import { cva, type VariantProps } from 'class-variance-authority';
import { cn } from '@/lib/utils';

const badgeVariants = cva(
  'inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px] font-medium leading-none transition-colors',
  {
    variants: {
      tone: {
        neutral: 'border-border bg-card text-muted-foreground',
        positive: 'border-emerald-500/20 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400',
        warning: 'border-amber-500/20 bg-amber-500/10 text-amber-700 dark:text-amber-400',
        danger: 'border-destructive/20 bg-destructive/10 text-destructive',
        info: 'border-sky-500/20 bg-sky-500/10 text-sky-700 dark:text-sky-400',
        accent: 'border-primary/20 bg-primary/10 text-primary',
      },
    },
    defaultVariants: { tone: 'neutral' },
  },
);

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, tone, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ tone }), className)} {...props} />;
}
