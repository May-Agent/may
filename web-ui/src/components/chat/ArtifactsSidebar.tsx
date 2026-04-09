import ReactMarkdown from 'react-markdown';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { X, History, ExternalLink } from 'lucide-react';
import type { Artifact, ArtifactPreviewTarget } from '@/types';
import { cn } from '@/lib/utils';

type PreviewPaneState = {
  kind: 'iframe' | 'image' | 'audio' | 'markdown' | 'text' | 'external' | 'slidev';
  url?: string;
  markdown?: string;
  text?: string;
};

interface ArtifactsSidebarProps {
  open: boolean;
  artifacts: Artifact[];
  selectedPath: string | null;
  preview: PreviewPaneState | null;
  showHistory: boolean;
  onClose: () => void;
  onSelect: (target: ArtifactPreviewTarget) => void;
  getViewerForPath?: (path: string) => ArtifactPreviewTarget['viewer'];
  onToggleHistory: () => void;
}

export function ArtifactsSidebar({
  open,
  artifacts,
  selectedPath,
  preview,
  showHistory,
  onClose,
  onSelect,
  getViewerForPath,
  onToggleHistory,
}: ArtifactsSidebarProps) {
  if (!open) return null;

  return (
    <div className="artifacts-shell flex h-full w-full min-w-0 flex-col border-l border-border/70 bg-background/95 text-foreground">
      <div className="artifacts-topbar flex items-center justify-between border-b border-border/70 px-4 py-3">
        <div className="text-base font-semibold text-foreground">Artifacts</div>
        <div className="flex items-center gap-2">
          {artifacts.length > 1 && (
            <Button
              variant="ghost"
              size="sm"
              className="artifacts-topbar-action h-8 gap-1.5 rounded-md px-2 text-muted-foreground hover:text-foreground"
              onClick={onToggleHistory}
            >
              <History className="h-4 w-4" />
              {showHistory ? 'Hide list' : 'History'}
            </Button>
          )}
            <Button
              variant="ghost"
              size="icon"
              className="artifacts-topbar-action h-8 w-8 rounded-md text-muted-foreground hover:text-foreground"
              onClick={onClose}
            >
              <X className="h-4 w-4" />
            </Button>
          </div>
      </div>

      <div className="flex min-h-0 flex-1">
        {showHistory && (
          <div className="artifacts-history-panel flex w-72 min-w-0 flex-col border-r border-border/70">
            <ScrollArea className="flex-1">
              <div className="space-y-1 p-2">
                {artifacts.map((artifact) => {
                  const viewer = getViewerForPath?.(artifact.path);
                  const isSelected = artifact.path === selectedPath;
                  return (
                    <button
                      key={artifact.path}
                      type="button"
                      className={cn(
                        'artifacts-history-item w-full rounded-lg border px-2.5 py-2.5 text-left transition-colors',
                        isSelected
                          ? 'border-cyan-500/35 bg-cyan-500/10 text-cyan-900 dark:text-cyan-100'
                          : 'border-border/70 bg-background/45 text-foreground/78 hover:border-foreground/15 hover:bg-muted/55 hover:text-foreground',
                      )}
                      onClick={() => onSelect({ path: artifact.path, viewer })}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="block truncate text-[13px] font-medium">{artifact.name}</span>
                        {viewer === 'slidev' && (
                          <span className="shrink-0 rounded-full border border-cyan-500/35 bg-cyan-500/12 px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-[0.14em] text-cyan-800 dark:text-cyan-100">
                            Slidev
                          </span>
                        )}
                      </div>
                      <span className="mt-0.5 block truncate text-[11px] text-muted-foreground">{artifact.path}</span>
                    </button>
                  );
                })}
              </div>
            </ScrollArea>
          </div>
        )}

        <div className="flex min-w-0 flex-1 flex-col">
          <div className="artifacts-preview-bar flex items-center justify-between border-b border-border/70 px-4 py-3">
            <div className="artifacts-preview-path truncate text-sm">
              {selectedPath ?? 'Preview'}
            </div>
          </div>
          <div className="flex-1 overflow-auto p-3.5">
            {!preview && (
              <div className="text-sm text-muted-foreground">No artifact preview available.</div>
            )}

            {preview?.kind === 'markdown' && (
              <div className="prose prose-sm dark:prose-invert max-w-none text-foreground">
                <ReactMarkdown>{preview.markdown ?? ''}</ReactMarkdown>
              </div>
            )}

            {preview?.kind === 'text' && (
              <pre className="whitespace-pre-wrap break-words text-xs text-foreground">{preview.text}</pre>
            )}

            {preview?.kind === 'slidev' && preview.url && (
              <div className="slidev-shell flex h-full min-h-[500px] flex-col overflow-hidden rounded-xl border">
                <div className="slidev-toolbar flex items-center justify-between border-b px-3 py-2.5">
                  <div className="flex items-center gap-2">
                    <span className="slidev-toolbar-title text-xs font-medium">Slidev</span>
                    <span className="slidev-toolbar-live text-[10px] uppercase tracking-[0.12em]">
                      live
                    </span>
                  </div>
                  <a
                    href={preview.url}
                    target="_blank"
                    rel="noreferrer"
                    className="slidev-toolbar-open inline-flex items-center gap-1 rounded-md border px-2 py-1 text-[11px] text-muted-foreground transition-colors hover:text-foreground"
                  >
                    <ExternalLink className="h-3 w-3" />
                    Open
                  </a>
                </div>
                <div className="slidev-viewport flex-1 overflow-hidden">
                  <div className="slidev-stage">
                    <iframe
                      key={preview.url}
                      title="slidev-preview"
                      src={preview.url}
                      className="slidev-frame"
                      allow="fullscreen; clipboard-read; clipboard-write; autoplay"
                      loading="eager"
                      allowFullScreen
                    />
                  </div>
                </div>
              </div>
            )}
            {preview?.kind === 'slidev' && !preview.url && (
              <div className="text-sm text-muted-foreground">Slidev preview is not available.</div>
            )}

            {(preview?.kind === 'iframe' || preview?.kind === 'external') && preview.url && (
              <div className="h-full min-h-[420px] overflow-hidden rounded-xl border border-border/70 bg-background/80 shadow-[0_18px_30px_-24px_rgba(15,23,42,0.22)]">
                <iframe
                  title="artifact-preview"
                  src={preview.url}
                  className="h-full min-h-[420px] w-full bg-background"
                />
              </div>
            )}

            {preview?.kind === 'image' && preview.url && (
              <img
                src={preview.url}
                alt={selectedPath ?? 'artifact'}
                className="h-auto max-w-full rounded-xl border border-border/70"
              />
            )}

            {preview?.kind === 'audio' && preview.url && (
              <audio src={preview.url} controls className="w-full" />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
