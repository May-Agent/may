import { AlertTriangle } from 'lucide-react';

/**
 * A warning for the user in the message stream, such as an abnormally long CI
 * job: a warning icon and amber text. It is not part of the agent's context.
 */
export function WarningMessage({ message }: { message: string }) {
  return (
    <div
      role="status"
      data-testid="warning-message"
      className="flex items-start gap-2 text-[13px] text-amber-600 dark:text-amber-400"
    >
      <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
      <p className="min-w-0 whitespace-pre-wrap break-words">{message}</p>
    </div>
  );
}
