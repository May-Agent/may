import { forwardRef, type HTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

export const ScrollArea = forwardRef<HTMLDivElement, HTMLAttributes<HTMLDivElement>>(
  ({ className, children, ...props }, ref) => (
    <div
      ref={ref}
      className={cn('scrollbar-thin overflow-y-auto overflow-x-hidden', className)}
      {...props}
    >
      {children}
    </div>
  ),
);
ScrollArea.displayName = 'ScrollArea';
