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
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const reportedRef = useRef(new Set<string>());
  const [diagnostics, setDiagnostics] = useState<string[] | null>(null);
  const [repairScheduled, setRepairScheduled] = useState(false);

  const resetValidation = useCallback(() => {
    setDiagnostics(null);
    setRepairScheduled(false);
    reportedRef.current.clear();
  }, []);

  const publishDiagnostics = useCallback(
    async (nextDiagnostics: string[]) => {
      setDiagnostics(nextDiagnostics);
      if (nextDiagnostics.length === 0) return;
      const reportKey = nextDiagnostics.join('\n');
      if (reportedRef.current.has(reportKey)) return;
      reportedRef.current.add(reportKey);
      try {
        const response = await reportArtifactDiagnostics(
          sessionId,
          path,
          artifactType,
          nextDiagnostics,
        );
        setRepairScheduled(response.repairScheduled);
      } catch (error) {
        const message =
          error instanceof Error ? error.message : 'Failed to report artifact diagnostics';
        setDiagnostics((current) => [...(current ?? []), message]);
      }
    },
    [artifactType, path, sessionId],
  );

  useEffect(() => {
    resetValidation();
  }, [artifactType, path, resetValidation, sessionId]);

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

  return {
    diagnostics,
    iframeRef,
    publishDiagnostics,
    repairScheduled,
    resetValidation,
  };
}
