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
    <div className="artifacts-shell flex h-full w-full min-w-0 flex-col border-l border-zinc-800/80 bg-zinc-950/95">
      <div className="artifacts-topbar flex items-center justify-between border-b border-zinc-800/75 px-4 py-3">
        <div className="text-base font-semibold text-zinc-100">Artifacts</div>
        <div className="flex items-center gap-2">
          {artifacts.length > 1 && (
            <Button
              variant="ghost"
              size="sm"
              className="artifacts-topbar-action h-8 gap-1.5 rounded-md px-2 text-zinc-300 hover:bg-zinc-800/70 hover:text-zinc-100"
              onClick={onToggleHistory}
            >
              <History className="h-4 w-4" />
              {showHistory ? 'Hide list' : 'History'}
            </Button>
          )}
            <Button
              variant="ghost"
              size="icon"
              className="artifacts-topbar-action h-8 w-8 rounded-md text-zinc-300 hover:bg-zinc-800/70 hover:text-zinc-100"
              onClick={onClose}
            >
              <X className="h-4 w-4" />
            </Button>
          </div>
      </div>

      <div className="flex min-h-0 flex-1">
        {showHistory && (
          <div className="artifacts-history-panel flex w-72 min-w-0 flex-col border-r border-zinc-800/75">
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
                          ? 'border-cyan-300/45 bg-cyan-500/10 text-cyan-100'
                          : 'border-zinc-800/80 bg-zinc-900/40 text-zinc-300 hover:border-zinc-600/80 hover:bg-zinc-800/60 hover:text-zinc-100',
                      )}
                      onClick={() => onSelect({ path: artifact.path, viewer })}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="block truncate text-[13px] font-medium">{artifact.name}</span>
                        {viewer === 'slidev' && (
                          <span className="shrink-0 rounded-full border border-cyan-300/45 bg-cyan-500/15 px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-[0.14em] text-cyan-100">
                            Slidev
                          </span>
                        )}
                      </div>
                      <span className="mt-0.5 block truncate text-[11px] text-zinc-500">{artifact.path}</span>
                    </button>
                  );
                })}
              </div>
            </ScrollArea>
          </div>
        )}

        <div className="flex min-w-0 flex-1 flex-col">
          <div className="artifacts-preview-bar flex items-center justify-between border-b border-zinc-800/75 px-4 py-3">
            <div className="artifacts-preview-path truncate text-sm text-zinc-400">
              {selectedPath ?? 'Preview'}
            </div>
          </div>
          <div className="flex-1 overflow-auto p-3.5">
            {!preview && (
              <div className="text-sm text-zinc-500">No artifact preview available.</div>
            )}

            {preview?.kind === 'markdown' && (
              <div className="prose prose-sm dark:prose-invert max-w-none text-zinc-200">
                <ReactMarkdown>{preview.markdown ?? ''}</ReactMarkdown>
              </div>
            )}

            {preview?.kind === 'text' && (
              <pre className="whitespace-pre-wrap break-words text-xs text-zinc-200">{preview.text}</pre>
            )}

            {preview?.kind === 'slidev' && preview.url && (
              <div className="slidev-shell flex h-full min-h-[500px] flex-col overflow-hidden rounded-xl border">
                <div className="slidev-toolbar flex items-center justify-between border-b px-3 py-2.5">
                  <div className="flex items-center gap-2">
                    <span className="slidev-toolbar-title text-xs font-medium text-zinc-100">Slidev</span>
                    <span className="slidev-toolbar-live text-[10px] uppercase tracking-[0.12em] text-cyan-200">
                      live
                    </span>
                  </div>
                  <a
                    href={preview.url}
                    target="_blank"
                    rel="noreferrer"
                    className="slidev-toolbar-open inline-flex items-center gap-1 rounded-md border px-2 py-1 text-[11px] text-zinc-300 transition-colors hover:text-zinc-100"
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
              <div className="text-sm text-zinc-500">Slidev preview is not available.</div>
            )}

            {(preview?.kind === 'iframe' || preview?.kind === 'external') && preview.url && (
              <div className="h-full min-h-[420px] overflow-hidden rounded-xl border border-white/10 bg-zinc-900/60 shadow-[inset_0_1px_0_rgba(255,255,255,0.04)]">
                <iframe
                  title="artifact-preview"
                  src={preview.url}
                  className="h-full min-h-[420px] w-full bg-zinc-900"
                />
              </div>
            )}

            {preview?.kind === 'image' && preview.url && (
              <img
                src={preview.url}
                alt={selectedPath ?? 'artifact'}
                className="h-auto max-w-full rounded-xl border border-white/10"
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
