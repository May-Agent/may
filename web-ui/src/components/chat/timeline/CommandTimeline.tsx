import { useState } from 'react';
import { cn } from '@/lib/utils';
import type { CommandTimelineDisplayItem } from '@/lib/timeline';
import { CopyButton } from '../shared/CopyButton';
import { buildSessionOutputPreviewUrl } from '@/lib/api';

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

function TerminalPanel({
  command,
  stdout,
  stderr,
  fallbackOutput,
  exitCode,
}: {
  command: string;
  stdout?: string;
  stderr?: string;
  fallbackOutput?: string;
  exitCode?: number;
}) {
  const body = stdout ?? fallbackOutput;
  const copyText = [
    `$ ${command}`,
    body?.trimEnd() ?? '',
    stderr?.trimEnd() ?? '',
    typeof exitCode === 'number' && exitCode !== 0 ? `exit ${exitCode}` : '',
  ]
    .filter((segment) => segment.length > 0)
    .join('\n\n');

  return (
    <div className="group/code terminal-panel relative overflow-hidden rounded-2xl border border-border/60">
      <CopyButton
        text={copyText}
        className="absolute right-2 top-2 z-10 h-7 w-7 text-muted-foreground opacity-0 transition-opacity group-hover/code:opacity-100 hover:bg-foreground/10"
      />
      <div className="max-h-[360px] overflow-y-auto px-4 py-3 scrollbar-none">
        <div className="whitespace-pre-wrap break-words font-mono text-[12px] leading-[1.55]">
          <span className="terminal-prompt">$&nbsp;</span>
          <span>{command}</span>
        </div>
        {body && (
          <pre className="mt-2 whitespace-pre-wrap break-words font-mono text-[12px] leading-[1.55]">
            {body}
          </pre>
        )}
        {stderr && (
          <pre className="terminal-stderr mt-2 whitespace-pre-wrap break-words font-mono text-[12px] leading-[1.55]">
            {stderr}
          </pre>
        )}
        {typeof exitCode === 'number' && exitCode !== 0 && (
          <div className="terminal-stderr mt-2 font-mono text-[11px] font-medium">
            exit {exitCode}
          </div>
        )}
      </div>
    </div>
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

  const storedPreviewItems = parsedOutput.storagePaths.map((storagePath) => {
    const src = sessionId ? buildSessionOutputPreviewUrl(sessionId, storagePath) : undefined;
    return { storagePath, src };
  });

  return (
    <div className="space-y-2">
      <TerminalPanel
        command={item.commandInput}
        stdout={parsedOutput.stdoutText}
        stderr={parsedOutput.stderrText}
        fallbackOutput={
          !parsedOutput.stdoutText && !parsedOutput.stderrText
            ? item.outputText
            : undefined
        }
        exitCode={parsedOutput.exitCode}
      />
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
