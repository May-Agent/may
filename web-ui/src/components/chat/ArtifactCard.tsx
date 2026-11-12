import { Download, ExternalLink, FileText } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { artifactDownloadUrl, artifactPageUrl } from '@/lib/api/artifacts';

interface ArtifactCardProps {
  sessionId: string;
  path: string;
}

/** Inline card for an `<artifact url="…" />` tag: opens the preview page. */
export function ArtifactCard({ sessionId, path }: ArtifactCardProps) {
  const name = path.split('/').at(-1) ?? path;
  return (
    <section className="not-prose mt-3 overflow-hidden rounded-xl border border-border bg-card/40">
      <div className="flex items-center gap-3 px-4 py-3">
        <div className="flex size-9 items-center justify-center rounded-md bg-muted text-muted-foreground">
          <FileText className="size-4.5" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{name}</div>
          <div className="truncate text-xs text-muted-foreground">{path}</div>
        </div>
        <Button size="sm" variant="ghost" asChild>
          <a href={artifactPageUrl(sessionId, path)} target="_blank" rel="noreferrer">
            <ExternalLink className="size-3.5" />
            Open
          </a>
        </Button>
        <Button size="sm" variant="ghost" asChild aria-label="Download">
          <a href={artifactDownloadUrl(sessionId, path)}>
            <Download className="size-3.5" />
          </a>
        </Button>
      </div>
    </section>
  );
}
