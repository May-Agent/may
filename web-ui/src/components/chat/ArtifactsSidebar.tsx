import ReactMarkdown from 'react-markdown';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import type { Artifact } from '@/types';

type PreviewPaneState = {
  kind: 'iframe' | 'image' | 'audio' | 'markdown' | 'text' | 'external';
  url?: string;
  markdown?: string;
  text?: string;
};

interface ArtifactsSidebarProps {
  open: boolean;
  artifacts: Artifact[];
  selectedPath: string | null;
  preview: PreviewPaneState | null;
  onClose: () => void;
  onReload: () => void;
  onSelect: (path: string) => void;
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
  return (
    <Sheet open={open} onOpenChange={(next) => !next && onClose()}>
      <SheetContent side="right" className="w-[680px] sm:max-w-[680px] p-0">
        <div className="h-full flex">
          <div className="w-72 border-r flex flex-col">
            <SheetHeader className="px-4 py-3 border-b">
              <SheetTitle className="text-base">Artifacts</SheetTitle>
            </SheetHeader>
            <ScrollArea className="flex-1">
              <div className="p-2 space-y-1">
                {artifacts.map((artifact) => (
                  <Button
                    key={artifact.path}
                    variant={artifact.path === selectedPath ? 'secondary' : 'ghost'}
                    className="w-full justify-start text-left"
                    onClick={() => onSelect(artifact.path)}
                  >
                    <span className="truncate">{artifact.name}</span>
                  </Button>
                ))}
              </div>
            </ScrollArea>
          </div>

          <div className="flex-1 flex flex-col min-w-0">
            <div className="px-4 py-3 border-b flex items-center justify-between">
              <div className="text-sm text-muted-foreground truncate">{selectedPath ?? 'No artifact selected'}</div>
              <Button variant="outline" size="sm" onClick={onReload} disabled={!selectedPath}>
                Reload
              </Button>
            </div>
            <div className="flex-1 overflow-auto p-4">
              {!preview && <div className="text-sm text-muted-foreground">Select an artifact to preview.</div>}

              {preview?.kind === 'markdown' && (
                <div className="prose prose-sm dark:prose-invert max-w-none">
                  <ReactMarkdown>{preview.markdown ?? ''}</ReactMarkdown>
                </div>
              )}

              {preview?.kind === 'text' && (
                <pre className="text-xs whitespace-pre-wrap break-words">{preview.text}</pre>
              )}

              {(preview?.kind === 'iframe' || preview?.kind === 'external') && preview.url && (
                <iframe title="artifact-preview" src={preview.url} className="w-full h-full min-h-[420px] border rounded" />
              )}

              {preview?.kind === 'image' && preview.url && (
                <img src={preview.url} alt={selectedPath ?? 'artifact'} className="max-w-full h-auto" />
              )}

              {preview?.kind === 'audio' && preview.url && (
                <audio src={preview.url} controls className="w-full" />
              )}
            </div>
          </div>
        </div>
      </SheetContent>
    </Sheet>
  );
}
