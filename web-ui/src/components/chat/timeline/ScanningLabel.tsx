import { cn } from '@/lib/utils';

/**
 * A label with a left-to-right scanning gradient and subtle glow,
 * used for running command descriptions and thinking indicators.
 */
export function ScanningLabel({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span className={cn('relative inline-block', className)}>
      <span className="scanning-label-glow" aria-hidden>
        {children}
      </span>
      <span className="scanning-label">{children}</span>
    </span>
  );
}
