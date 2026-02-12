import { useMemo } from 'react';
import { X, RefreshCw, Globe, FileCode, FileText } from 'lucide-react';
import ReactMarkdown from 'react-markdown';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import type { Artifact } from '@/types';

interface ArtifactsSidebarProps {
  open: boolean;
  artifacts: Artifact[];
  selectedPath: string | null;
  preview: {
    kind: 'iframe' | 'image' | 'audio' | 'markdown' | 'text' | 'external';
    url?: string;
    markdown?: string;
    text?: string;
  } | null;
  onClose: () => void;
  onReload: () => void;
  onSelect: (path: string) => void;
}

function isExternalUrl(value: string): boolean {
  return value.startsWith('http://') || value.startsWith('https://');
}

export function ArtifactsSidebar({
  open,
  artifacts,
  selectedPath,
  preview,
  onClose,
  onReload,
  onSelect,
}: ArtifactsSidebarProps) {
  const hasExternal = !!selectedPath && isExternalUrl(selectedPath);
  const selectedName = useMemo(() => {
    if (!selectedPath) return '';
    const parts = selectedPath.split('/').filter(Boolean);
    return parts[parts.length - 1] || selectedPath;
  }, [selectedPath]);

  return (
    <aside
      className={`h-full border-l bg-background transition-all duration-200 ease-out ${open ? 'w-[42%] min-w-[360px] max-w-[760px]' : 'w-0 border-l-0'}`}
      aria-hidden={!open}
    >
      {open && (
        <div className="flex h-full flex-col">
          <div className="flex items-center justify-between border-b px-3 py-2">
            <div className="min-w-0">
              <div className="truncate text-sm font-medium">{selectedName || 'Preview'}</div>
              <div className="text-xs text-muted-foreground">Artifacts</div>
            </div>
            <div className="flex items-center gap-1">
              <Button variant="ghost" size="icon" className="h-8 w-8" onClick={onReload} title="Reload preview">
                <RefreshCw className="h-4 w-4" />
              </Button>
              <Button variant="ghost" size="icon" className="h-8 w-8" onClick={onClose} title="Close preview">
                <X className="h-4 w-4" />
              </Button>
            </div>
          </div>

          <div className="grid min-h-0 flex-1 grid-cols-[260px_minmax(0,1fr)]">
            <div className="border-r">
              <ScrollArea className="h-full">
                <div className="space-y-1 p-2">
                  {artifacts.map((artifact) => {
                    const selected = selectedPath === artifact.path;
                    return (
                      <button
                        key={artifact.path}
                        type="button"
                        onClick={() => onSelect(artifact.path)}
                        className={`w-full rounded-md border px-2 py-2 text-left transition-colors ${selected ? 'border-primary/30 bg-primary/10' : 'border-transparent hover:bg-muted/60'}`}
                      >
                        <div className="flex items-center gap-2">
                          {artifact.isHtml ? (
                            <FileCode className="h-4 w-4 text-muted-foreground" />
                          ) : (
                            <FileText className="h-4 w-4 text-muted-foreground" />
                          )}
                          <span className="truncate text-sm">{artifact.name}</span>
                        </div>
                        <div className="truncate pl-6 text-[11px] text-muted-foreground">{artifact.path}</div>
                      </button>
                    );
                  })}
                  {artifacts.length === 0 && (
                    <div className="rounded-md border border-dashed px-3 py-4 text-xs text-muted-foreground">
                      No artifacts yet.
                    </div>
                  )}
                </div>
              </ScrollArea>
            </div>

            <div className="min-h-0 bg-muted/20">
              {!preview && (
                <div className="flex h-full items-center justify-center px-6 text-sm text-muted-foreground">
                  {hasExternal ? (
                    <a href={selectedPath || '#'} target="_blank" rel="noreferrer" className="inline-flex items-center gap-2 underline">
                      <Globe className="h-4 w-4" />
                      Open external preview
                    </a>
                  ) : (
                    'Select an artifact or use <preview> to open one.'
                  )}
                </div>
              )}

              {preview?.kind === 'markdown' && (
                <ScrollArea className="h-full">
                  <div className="prose prose-sm dark:prose-invert max-w-none p-4">
                    <ReactMarkdown>{preview.markdown || ''}</ReactMarkdown>
                  </div>
                </ScrollArea>
              )}

              {preview?.kind === 'text' && (
                <ScrollArea className="h-full">
                  <pre className="whitespace-pre-wrap break-words p-4 text-sm text-foreground">{preview.text || ''}</pre>
                </ScrollArea>
              )}

              {preview?.kind === 'image' && preview.url && (
                <div className="flex h-full items-center justify-center p-3">
                  <img src={preview.url} alt={selectedName || 'preview'} className="max-h-full max-w-full object-contain" />
                </div>
              )}

              {preview?.kind === 'audio' && preview.url && (
                <div className="flex h-full items-center justify-center p-6">
                  <audio src={preview.url} controls className="w-full" />
                </div>
              )}

              {(preview?.kind === 'iframe' || preview?.kind === 'external') && preview.url && (
                <iframe
                  key={preview.url}
                  src={preview.url}
                  className="h-full w-full border-0 bg-white"
                  sandbox={preview.kind === 'external' ? undefined : 'allow-scripts allow-same-origin allow-downloads allow-forms'}
                  title="Artifact Preview"
                />
              )}
            </div>
          </div>
        </div>
      )}
    </aside>
  );
}
