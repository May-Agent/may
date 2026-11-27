import type { CommandReading, CommandStep, DiffHunk, DiffLine, FileEdit } from '../../lib/types';

const LINK_MARKER: Record<CommandStep['link'], string> = {
  start: '',
  pipe: '|',
  and: '&&',
  or: '||',
  then: ';',
};

function fileVerb(file: FileEdit): { label: string; className: string } {
  switch (file.change.kind) {
    case 'add':
      return { label: 'Created', className: 'text-emerald-700 dark:text-emerald-400' };
    case 'delete':
      return { label: 'Deleted', className: 'text-red-700 dark:text-red-400' };
    case 'update':
      return { label: 'Edited', className: 'text-cyan-700 dark:text-cyan-400' };
  }
}

function fileTarget(file: FileEdit): string {
  if (file.change.kind === 'update' && file.change.movedTo) {
    return `${file.path} → ${file.change.movedTo}`;
  }
  return file.path;
}

function fileHunks(file: FileEdit): DiffHunk[] {
  switch (file.change.kind) {
    case 'add':
      return [{ lines: file.change.lines }];
    case 'delete':
      return [];
    case 'update':
      return file.change.hunks;
  }
}

const SIGN: Record<DiffLine['op'], string> = { context: ' ', added: '+', removed: '-' };
const OP_CLASS: Record<DiffLine['op'], string> = {
  context: 'text-muted-foreground',
  added: 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-400',
  removed: 'bg-red-500/10 text-red-700 dark:text-red-400',
};

function DiffRow({ line }: { line: DiffLine }) {
  return (
    <div className={`flex whitespace-pre-wrap break-all ${OP_CLASS[line.op]}`} data-op={line.op}>
      <span className="w-9 shrink-0 select-none pr-1 text-right text-muted-foreground/70">
        {line.old ?? ''}
      </span>
      <span className="w-9 shrink-0 select-none pr-1 text-right text-muted-foreground/70">
        {line.new ?? ''}
      </span>
      <span className="w-4 shrink-0 select-none">{SIGN[line.op]}</span>
      <span className="min-w-0">{line.text}</span>
    </div>
  );
}

function FileDiff({ file }: { file: FileEdit }) {
  const verb = fileVerb(file);
  return (
    <div className="space-y-0.5">
      <div className="font-medium text-foreground/90">
        <span className={verb.className}>{verb.label}</span> {fileTarget(file)}
      </div>
      {fileHunks(file).map((hunk, index) => (
        <div key={index} className="rounded border border-border/60 bg-muted/20 py-0.5">
          {hunk.anchor && (
            <div className="px-2 text-muted-foreground">{`@@ ${hunk.anchor}`}</div>
          )}
          {hunk.lines.map((line, row) => (
            <DiffRow key={row} line={line} />
          ))}
        </div>
      ))}
    </div>
  );
}

/**
 * A terminal script rendered from how the daemon read it: one line in plain
 * words per recognised command, a line-numbered diff under every edit, and
 * the raw command for a part nothing recognised.
 */
export function CommandReadingView({ reading }: { reading: CommandReading }) {
  return (
    <div className="space-y-1 text-xs font-mono leading-relaxed" data-testid="command-reading">
      {reading.steps.map((step, index) => {
        const singleFileEdit = step.intent.kind === 'edit' && step.intent.files.length === 1;
        return (
          <div key={index} className="space-y-1">
            {!singleFileEdit && (
              <div className="flex gap-2">
                <span className="w-5 shrink-0 select-none text-muted-foreground">
                  {LINK_MARKER[step.link]}
                </span>
                {step.intent.kind === 'unknown' ? (
                  <span className="min-w-0 whitespace-pre-wrap break-all text-amber-700 dark:text-amber-400">
                    {`$ ${step.intent.source}`}
                  </span>
                ) : (
                  <span className="min-w-0 text-foreground/90">{step.line}</span>
                )}
              </div>
            )}
            {step.intent.kind === 'edit' && (
              <div className={singleFileEdit ? 'flex gap-2' : 'ml-7 space-y-1.5'}>
                {singleFileEdit && (
                  <span className="w-5 shrink-0 select-none text-muted-foreground">
                    {LINK_MARKER[step.link]}
                  </span>
                )}
                <div className="min-w-0 flex-1 space-y-1.5">
                  {step.intent.files.map((file, fileIndex) => (
                    <FileDiff key={fileIndex} file={file} />
                  ))}
                </div>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
