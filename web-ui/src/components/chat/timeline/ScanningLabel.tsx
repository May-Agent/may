import { cn } from '@/lib/utils';

/**
 * A label with a left-to-right scanning gradient, used for running command descriptions.
 * Gives a terminal-style "working" feel without being distracting.
 */
export function ScanningLabel({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span className={cn('scanning-label', className)}>
      {children}
    </span>
  );
}
