import type { HTMLAttributes } from 'react';
import { cva, type VariantProps } from 'class-variance-authority';
import { cn } from '@/lib/utils';

// Status reads as plain secondary text, never as a colored capsule; only a
// failure is red. The tones are kept so callers keep stating intent.
const badgeVariants = cva('inline-flex items-center gap-1 text-xs leading-none', {
  variants: {
    tone: {
      neutral: 'text-muted-foreground',
      positive: 'text-muted-foreground',
      warning: 'text-muted-foreground',
      danger: 'text-destructive',
      info: 'text-muted-foreground',
      accent: 'text-foreground',
    },
  },
  defaultVariants: { tone: 'neutral' },
});

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, tone, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ tone }), className)} {...props} />;
}
