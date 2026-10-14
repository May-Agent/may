import { useCallback, useEffect, useRef, useState } from 'react';
import { reportArtifactDiagnostics } from '@/lib/api/artifacts';
import { parseArtifactValidationMessage } from '@/lib/artifactValidation';

interface ArtifactValidationOptions {
  sessionId: string;
  path: string;
  artifactType: string;
}

export function useArtifactValidation({
  sessionId,
  path,
  artifactType,
}: ArtifactValidationOptions) {
  const artifactKey = `${artifactType}:${path}:${sessionId}`;
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const reportedRef = useRef<{ key: string; reported: Set<string> }>({
    key: artifactKey,
    reported: new Set(),
  });
  // Validation state is keyed by artifact identity so a sessionId/path switch
  // derives empty state instead of requiring an effect-driven reset.
  const [validation, setValidation] = useState<{
    key: string;
    diagnostics: string[] | null;
    repairScheduled: boolean;
  }>({ key: artifactKey, diagnostics: null, repairScheduled: false });

  const publishDiagnostics = useCallback(
    async (nextDiagnostics: string[]) => {
      setValidation({ key: artifactKey, diagnostics: nextDiagnostics, repairScheduled: false });
      if (nextDiagnostics.length === 0) return;
      if (reportedRef.current.key !== artifactKey) {
        reportedRef.current = { key: artifactKey, reported: new Set() };
      }
      const reportKey = nextDiagnostics.join('\n');
      if (reportedRef.current.reported.has(reportKey)) return;
      reportedRef.current.reported.add(reportKey);
      try {
        const response = await reportArtifactDiagnostics(
          sessionId,
          path,
          artifactType,
          nextDiagnostics,
        );
        setValidation((current) =>
          current.key === artifactKey
            ? { ...current, repairScheduled: response.repairScheduled }
            : current,
        );
      } catch (error) {
        const message =
          error instanceof Error ? error.message : 'Failed to report artifact diagnostics';
        setValidation((current) =>
          current.key === artifactKey
            ? { ...current, diagnostics: [...(current.diagnostics ?? []), message] }
            : current,
        );
      }
    },
    [artifactKey, artifactType, path, sessionId],
  );

  useEffect(() => {
    const onMessage = (event: MessageEvent<unknown>) => {
      if (event.source !== iframeRef.current?.contentWindow) return;
      const message = parseArtifactValidationMessage(event.data, artifactType);
      if (!message) return;
      void publishDiagnostics(message.diagnostics);
    };
    window.addEventListener('message', onMessage);
    return () => window.removeEventListener('message', onMessage);
  }, [artifactType, publishDiagnostics]);

  const current = validation.key === artifactKey ? validation : null;
  return {
    diagnostics: current?.diagnostics ?? null,
    iframeRef,
    publishDiagnostics,
    repairScheduled: current?.repairScheduled ?? false,
  };
}
