import type { PersistedPermissionRequest } from '../../lib/types';
import { recognisesAny } from '../../lib/commandReading';
import { CommandReadingView } from '../chat/CommandReadingView';

interface PermissionBodyProps {
  request: PersistedPermissionRequest;
  /** Show the raw script instead of the reading. */
  showScript: boolean;
}

/**
 * What an approval prompt shows of the request: how the daemon read the
 * script (the file and diff for an edit), or the raw script when the
 * reading recognised nothing or the user asked for it.
 */
export function PermissionBody({ request, showScript }: PermissionBodyProps) {
  const reading = request.reading;
  if (recognisesAny(reading) && !showScript) {
    return (
      <div className="rounded-md border border-border bg-muted/40 px-3 py-2">
        <CommandReadingView reading={reading} />
      </div>
    );
  }
  return (
    <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono whitespace-pre-wrap break-all">
      {request.script}
    </pre>
  );
}
