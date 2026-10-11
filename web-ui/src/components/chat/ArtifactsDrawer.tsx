import { useEffect, useState } from 'react';
import { Streamdown } from 'streamdown';
import {
  FileText,
  Image as ImageIcon,
  FileCode2,
  File,
  ExternalLink,
  Folder,
  Download,
  ArrowLeft,
} from 'lucide-react';
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetDescription } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import {
  artifactPreviewUrl,
  artifactRuntimePreviewUrl,
  listArtifacts,
  openArtifactStream,
} from '@/lib/api/artifacts';
import { useTranslation } from '@/lib/i18n';
import type { ArtifactEntry } from '@/lib/types';
import { cn } from '@/lib/utils';
import { useArtifactValidation } from '@/lib/useArtifactValidation';
import { ArtifactValidationStatus } from './ArtifactValidationStatus';

interface ArtifactsDrawerProps {
  sessionId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initialCount?: number;
}

export function ArtifactsDrawer({ sessionId, open, onOpenChange }: ArtifactsDrawerProps) {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<ArtifactEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<ArtifactEntry | null>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    const load = async () => {
      try {
        const res = await listArtifacts(sessionId);
        if (!cancelled) {
          setEntries(res.artifacts);
          setLoading(false);
        }
      } catch {
        if (!cancelled) setLoading(false);
      }
    };
    void load();

    const controller = new AbortController();
    (async () => {
      try {
        const handle = await openArtifactStream(sessionId, controller.signal);
        for await (const evt of handle.events) {
          if (cancelled) break;
          if (evt.kind === 'artifacts') {
            void load();
          }
        }
      } catch {
        // stream aborted
      }
    })();

    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [sessionId, open]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="right" className="w-full max-w-lg">
        <SheetHeader className="pr-10">
          <div className="flex items-center gap-2">
            {selected && (
              <button
                type="button"
                className="rounded-md p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                onClick={() => setSelected(null)}
                aria-label={t('common.back')}
              >
                <ArrowLeft className="size-3.5" />
              </button>
            )}
            <SheetTitle>
              {selected ? selected.name : t('chat.artifacts')}
            </SheetTitle>
          </div>
          <SheetDescription>
            {selected
              ? formatBytes(selected.size)
              : entries.length === 0
              ? t('chat.artifactEmpty')
              : `${entries.length} ${entries.length === 1 ? 'file' : 'files'} in this session`}
          </SheetDescription>
        </SheetHeader>

        <div className="flex-1 overflow-y-auto">
          {selected ? (
            <ArtifactPreview sessionId={sessionId} entry={selected} />
          ) : loading ? (
            <div className="py-10 text-center text-sm text-muted-foreground">
              {t('common.loading')}
            </div>
          ) : entries.length === 0 ? (
            <EmptyState />
          ) : (
            <ul className="divide-y divide-border">
              {entries.map((entry) => {
                const Icon = iconFor(entry);
                return (
                  <li key={entry.path}>
                    <button
                      type="button"
                      onClick={() => setSelected(entry)}
                      className="flex w-full items-center gap-3 px-5 py-3 text-left transition-colors hover:bg-accent/40"
                    >
                      <Icon className="size-4 shrink-0 text-muted-foreground" />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-sm">{entry.name}</div>
                        <div className="truncate text-[11px] text-muted-foreground">
                          {entry.path !== entry.name ? entry.path : ''}
                          {entry.path !== entry.name && <span className="mx-1">·</span>}
                          {formatBytes(entry.size)}
                        </div>
                      </div>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}

function EmptyState() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col items-center justify-center gap-2 py-16 text-sm text-muted-foreground">
      <Folder className="size-8 opacity-40" />
      <span>{t('chat.artifactEmpty')}</span>
      <p className="max-w-[20rem] text-center text-xs text-muted-foreground/80">
        Files May creates for you in this session will show up here.
      </p>
    </div>
  );
}

function ArtifactPreview({ sessionId, entry }: { sessionId: string; entry: ArtifactEntry }) {
  const url = artifactPreviewUrl(sessionId, entry.path);
  const ext = extOf(entry.name).toLowerCase();
  const kind = kindFor(entry, ext);

  if (kind === 'image') {
    return (
      <div className="flex flex-col items-center gap-3 p-5">
        <img
          src={url}
          alt={entry.name}
          className="max-h-[70vh] max-w-full rounded-md border border-border object-contain"
        />
        <PreviewActions url={url} name={entry.name} />
      </div>
    );
  }
  if (kind === 'html') {
    return <HtmlArtifactPreview sessionId={sessionId} entry={entry} downloadUrl={url} />;
  }
  if (kind === 'text' || kind === 'markdown' || kind === 'code') {
    return <TextPreview sessionId={sessionId} entry={entry} url={url} kind={kind} />;
  }

  return (
    <div className="flex flex-col items-center gap-3 p-10 text-sm text-muted-foreground">
      <File className="size-8 opacity-40" />
      <span>Preview isn’t available for this file type.</span>
      <PreviewActions url={url} name={entry.name} />
    </div>
  );
}

function HtmlArtifactPreview({
  sessionId,
  entry,
  downloadUrl,
}: {
  sessionId: string;
  entry: ArtifactEntry;
  downloadUrl: string;
}) {
  const artifactType = 'html';
  const runtimeUrl = artifactRuntimePreviewUrl(sessionId, entry.path, artifactType);
  const { diagnostics, iframeRef, repairScheduled } = useArtifactValidation({
    sessionId,
    path: entry.path,
    artifactType,
  });
  return (
    <div className="flex h-full flex-col">
      <iframe
        ref={iframeRef}
        src={runtimeUrl}
        title={entry.name}
        sandbox="allow-scripts"
        className="h-full w-full flex-1 border-0 bg-background"
      />
      <ArtifactValidationStatus
        diagnostics={diagnostics}
        repairScheduled={repairScheduled}
        pendingLabel="Waiting for artifact runtime validation…"
      />
      <div className="border-t border-border p-3">
        <PreviewActions url={downloadUrl} name={entry.name} />
      </div>
    </div>
  );
}

function PreviewActions({ url, name }: { url: string; name: string }) {
  return (
    <div className="flex items-center gap-2">
      <Button size="sm" variant="outline" asChild>
        <a href={url} target="_blank" rel="noreferrer">
          <ExternalLink className="size-3.5" />
          Open in new tab
        </a>
      </Button>
      <Button size="sm" variant="ghost" asChild>
        <a href={url} download={name}>
          <Download className="size-3.5" />
          Download
        </a>
      </Button>
    </div>
  );
}

function TextPreview({
  sessionId,
  entry,
  url,
  kind,
}: {
  sessionId: string;
  entry: ArtifactEntry;
  url: string;
  kind: 'text' | 'markdown' | 'code';
}) {
  const [content, setContent] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await fetch(url);
        if (!res.ok) throw new Error(`Failed: ${res.status}`);
        const text = await res.text();
        if (!cancelled) setContent(text);
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Preview failed');
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [url]);

  void sessionId;
  void entry;

  if (error) {
    return <div className="p-5 text-sm text-destructive">{error}</div>;
  }
  if (content === null) {
    return <div className="p-5 text-sm text-muted-foreground">Loading…</div>;
  }

  if (kind === 'markdown') {
    return (
      <div className="p-5">
        <div className="prose prose-sm dark:prose-invert max-w-none">
          <Streamdown>{content}</Streamdown>
        </div>
        <div className="mt-4 flex justify-center">
          <PreviewActions url={url} name={entry.name} />
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <pre
        className={cn(
          'flex-1 overflow-auto border-0 bg-card/20 p-5 font-mono text-xs leading-relaxed',
        )}
      >
        {content}
      </pre>
      <div className="border-t border-border p-3">
        <PreviewActions url={url} name={entry.name} />
      </div>
    </div>
  );
}

function extOf(name: string): string {
  const dot = name.lastIndexOf('.');
  return dot >= 0 ? name.slice(dot + 1) : '';
}

function iconFor(entry: ArtifactEntry) {
  if (entry.isHtml) return FileCode2;
  const ext = extOf(entry.name).toLowerCase();
  if (IMAGE_EXT.has(ext)) return ImageIcon;
  if (CODE_EXT.has(ext)) return FileCode2;
  if (TEXT_EXT.has(ext)) return FileText;
  return File;
}

type PreviewKind = 'html' | 'image' | 'markdown' | 'text' | 'code' | 'binary';

function kindFor(entry: ArtifactEntry, ext: string): PreviewKind {
  if (entry.isHtml || ext === 'html' || ext === 'htm') return 'html';
  if (IMAGE_EXT.has(ext)) return 'image';
  if (ext === 'md' || ext === 'markdown') return 'markdown';
  if (CODE_EXT.has(ext)) return 'code';
  if (TEXT_EXT.has(ext)) return 'text';
  return 'binary';
}

const IMAGE_EXT = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif']);
const CODE_EXT = new Set([
  'ts',
  'tsx',
  'js',
  'jsx',
  'py',
  'rs',
  'go',
  'java',
  'c',
  'cpp',
  'rb',
  'sh',
  'css',
  'scss',
  'json',
  'yaml',
  'yml',
  'toml',
]);
const TEXT_EXT = new Set(['txt', 'log', 'csv', 'tsv', 'ini', 'conf', 'env']);

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
