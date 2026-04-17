import { Suspense, lazy, useState } from 'react';
import { cn } from '@/lib/utils';
import type { CommandTimelineDisplayItem } from '@/lib/timeline';
import { CopyButton } from '../shared/CopyButton';
import { buildSessionOutputPreviewUrl } from '@/lib/api';

const LazySyntaxCodeBlock = lazy(() => import('../SyntaxCodeBlock'));

function StoredOutputImage({ src, alt }: { src: string; alt: string }) {
  const [failed, setFailed] = useState(false);
  if (failed) {
    return null;
  }
  return (
    <img
      src={src}
      alt={alt}
      className="h-auto max-h-[420px] w-full max-w-full rounded-lg border border-border/70 bg-muted/20 object-contain"
      loading="lazy"
      onError={() => setFailed(true)}
    />
  );
}

function OutputStreamPanel({
  label,
  text,
  tone = 'default',
}: {
  label: string;
  text: string;
  tone?: 'default' | 'stderr' | 'error';
}) {
  const toneClass =
    tone === 'stderr'
      ? 'text-destructive/80'
      : tone === 'error'
        ? 'text-destructive'
        : 'text-foreground';
  return (
    <section className="group/stream relative rounded-xl border border-border/60 bg-muted/10">
      <header className="flex items-center justify-between px-3 pb-1 pt-2">
        <span className="font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
          {label}
        </span>
        <CopyButton
          text={text}
          className="h-6 w-6 text-muted-foreground opacity-0 transition-opacity group-hover/stream:opacity-100 hover:bg-foreground/10"
        />
      </header>
      <pre
        className={cn(
          'max-h-[40vh] overflow-y-auto whitespace-pre-wrap break-words px-3 pb-3 font-mono text-[12px] leading-[1.55]',
          toneClass,
        )}
      >
        {text}
      </pre>
    </section>
  );
}

function CommandHeader({
  command,
  exitCode,
  isFailed,
}: {
  command: string;
  exitCode?: number;
  isFailed: boolean;
}) {
  const language = 'bash';
  return (
    <section className="group/cmd relative overflow-hidden rounded-xl border border-border/60 bg-background/60">
      <header className="flex items-center justify-between px-3 py-1.5 border-b border-border/40">
        <div className="flex items-center gap-2">
          <span className="font-mono text-[10px] uppercase tracking-wider text-muted-foreground">
            command
          </span>
          {isFailed && typeof exitCode === 'number' ? (
            <span className="inline-flex items-center rounded-md border border-destructive/40 bg-destructive/10 px-1.5 py-0.5 font-mono text-[10px] font-semibold text-destructive">
              exit {exitCode}
            </span>
          ) : null}
        </div>
        <CopyButton
          text={command}
          className="h-6 w-6 text-muted-foreground opacity-0 transition-opacity group-hover/cmd:opacity-100 hover:bg-foreground/10"
        />
      </header>
      <Suspense
        fallback={
          <pre className="m-0 overflow-x-auto bg-transparent px-3 py-2 font-mono text-[12px] leading-[1.55] text-foreground">
            <code>{command}</code>
          </pre>
        }
      >
        <LazySyntaxCodeBlock language={language} code={command} />
      </Suspense>
    </section>
  );
}

export function CommandTimeline({
  item,
  sessionId,
}: {
  item: CommandTimelineDisplayItem;
  sessionId?: string;
}) {
  const { parsedOutput } = item;
  const isFailed = item.status === 'failed' || parsedOutput.isError === true;

  const storedPreviewItems = parsedOutput.storagePaths.map((storagePath) => {
    const src = sessionId ? buildSessionOutputPreviewUrl(sessionId, storagePath) : undefined;
    return { storagePath, src };
  });

  const stdoutText = parsedOutput.stdoutText?.trimEnd();
  const stderrText = parsedOutput.stderrText?.trimEnd();
  const fallbackOutput =
    !stdoutText && !stderrText ? item.outputText?.trimEnd() : undefined;
  const showErrorMessage =
    typeof parsedOutput.errorMessage === 'string' &&
    !stdoutText &&
    !stderrText &&
    !fallbackOutput;

  return (
    <div className="space-y-2">
      <CommandHeader
        command={item.commandInput}
        exitCode={parsedOutput.exitCode}
        isFailed={isFailed}
      />
      {stdoutText && stdoutText.length > 0 ? (
        <OutputStreamPanel label="stdout" text={stdoutText} />
      ) : null}
      {stderrText && stderrText.length > 0 ? (
        <OutputStreamPanel label="stderr" text={stderrText} tone="stderr" />
      ) : null}
      {fallbackOutput && fallbackOutput.length > 0 ? (
        <OutputStreamPanel label="output" text={fallbackOutput} />
      ) : null}
      {showErrorMessage ? (
        <OutputStreamPanel
          label="error"
          text={parsedOutput.errorMessage ?? ''}
          tone="error"
        />
      ) : null}
      {(parsedOutput.images.length > 0 || storedPreviewItems.length > 0) && (
        <div className="grid gap-2">
          {parsedOutput.images.map((image, imageIndex) => (
            <figure
              key={`${item.id}-inline-image-${imageIndex}`}
              className="space-y-1 rounded-xl border border-border/70 bg-background/70 p-2"
            >
              <img
                src={image.src}
                alt={`Command output image (${image.mediaType})`}
                className={cn(
                  'h-auto max-h-[360px] w-full max-w-full rounded object-contain',
                )}
                loading="lazy"
              />
              <figcaption className="break-all font-mono text-[10px] text-muted-foreground">
                {image.mediaType}
              </figcaption>
            </figure>
          ))}
          {storedPreviewItems.map((previewItem, previewItemIndex) => (
            <div
              key={`${item.id}-stored-output-${previewItem.storagePath}-${previewItemIndex}`}
              className="space-y-1 rounded-xl border border-border/70 bg-background/70 p-2"
            >
              {previewItem.src ? (
                <StoredOutputImage
                  src={previewItem.src}
                  alt={`Stored command output ${previewItem.storagePath}`}
                />
              ) : null}
              <div className="break-all font-mono text-[10px] text-muted-foreground">
                {previewItem.storagePath}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
