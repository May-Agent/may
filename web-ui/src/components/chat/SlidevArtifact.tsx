import { useEffect, useState } from 'react';
import { ExternalLink, Presentation } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { prepareSlidevPreview } from '@/lib/api/artifacts';
import { useArtifactValidation } from '@/lib/useArtifactValidation';
import { ArtifactValidationStatus } from './ArtifactValidationStatus';

interface SlidevArtifactProps {
  sessionId: string;
  path: string;
}

export function SlidevArtifact({ sessionId, path }: SlidevArtifactProps) {
  const fetchKey = `${sessionId}:${path}`;
  // Preview state is keyed by fetch identity so a sessionId/path switch
  // derives empty state instead of requiring an effect-driven reset.
  const [preview, setPreview] = useState<{ key: string; url: string | null }>({
    key: fetchKey,
    url: null,
  });
  const previewUrl = preview.key === fetchKey ? preview.url : null;
  const { diagnostics, iframeRef, publishDiagnostics, repairScheduled } =
    useArtifactValidation({ sessionId, path, artifactType: 'slidev' });

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const response = await prepareSlidevPreview(sessionId, path);
        if (cancelled) return;
        if (response.diagnostics.length > 0) {
          await publishDiagnostics(response.diagnostics);
          return;
        }
        if (!response.previewUrl) {
          await publishDiagnostics(['Slidev preview completed without a preview URL.']);
          return;
        }
        setPreview({ key: fetchKey, url: response.previewUrl });
      } catch (error) {
        if (cancelled) return;
        const message = error instanceof Error ? error.message : 'Slidev preview failed';
        await publishDiagnostics([`Slidev preview failed: ${message}`]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [fetchKey, path, publishDiagnostics, sessionId]);

  return (
    <section className="not-prose mt-3 overflow-hidden rounded-xl border border-border bg-card/40">
      <div className="flex items-center gap-3 border-b border-border px-4 py-3">
        <div className="flex size-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <Presentation className="size-4.5" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{path.split('/').at(-1)}</div>
          <div className="truncate text-xs text-muted-foreground">Slidev · {path}</div>
        </div>
        {previewUrl && (
          <Button size="sm" variant="ghost" asChild>
            <a href={previewUrl} target="_blank" rel="noreferrer">
              <ExternalLink className="size-3.5" />
              Open
            </a>
          </Button>
        )}
      </div>

      {previewUrl && (
        <iframe
          ref={iframeRef}
          src={previewUrl}
          title={path}
          sandbox="allow-scripts"
          className="aspect-video w-full border-0 bg-background"
        />
      )}

      <ArtifactValidationStatus
        diagnostics={diagnostics}
        repairScheduled={repairScheduled}
        pendingLabel="Building and validating the artifact runtime…"
      />
    </section>
  );
}
