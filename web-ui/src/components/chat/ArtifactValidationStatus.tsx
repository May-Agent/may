import { AlertTriangle, CheckCircle2, LoaderCircle } from 'lucide-react';

interface ArtifactValidationStatusProps {
  diagnostics: string[] | null;
  repairScheduled: boolean;
  pendingLabel: string;
}

export function ArtifactValidationStatus({
  diagnostics,
  repairScheduled,
  pendingLabel,
}: ArtifactValidationStatusProps) {
  if (diagnostics === null) {
    return (
      <div className="flex items-center gap-2 px-4 py-3 text-xs text-muted-foreground">
        <LoaderCircle className="size-3.5 animate-spin" />
        {pendingLabel}
      </div>
    );
  }
  if (diagnostics.length === 0) {
    return (
      <div className="flex items-center gap-2 px-4 py-3 text-xs text-muted-foreground">
        <CheckCircle2 className="size-3.5" />
        Artifact runtime validated successfully.
      </div>
    );
  }
  return (
    <div className="border-t border-destructive/20 bg-destructive/5 px-4 py-3 text-xs text-destructive">
      <div className="mb-1.5 flex items-center gap-2 font-medium">
        <AlertTriangle className="size-3.5" />
        Artifact validation failed
      </div>
      <ul className="list-disc space-y-1 pl-5">
        {diagnostics.map((diagnostic) => (
          <li key={diagnostic}>{diagnostic}</li>
        ))}
      </ul>
      {repairScheduled && (
        <div className="mt-2 text-muted-foreground">May is repairing this artifact.</div>
      )}
    </div>
  );
}
