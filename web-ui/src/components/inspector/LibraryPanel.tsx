import { useEffect, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { openUiEventStream } from '@/lib/api/events';
import { useTranslation } from '@/lib/i18n';
import type { ArtifactEntry } from '@/lib/types';
import { ArtifactsDrawer } from '@/components/chat/ArtifactsDrawer';
import { formatBytes } from '@/lib/formatBytes';

/** The inspector's Library tab: the files May made in this chat. */
export function LibraryPanel({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<ArtifactEntry[] | null>(null);
  const [opened, setOpened] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    (async () => {
      try {
        const handle = await openUiEventStream(['session_artifacts'], {
          sessionId,
          abort: controller.signal,
        });
        for await (const evt of handle.events) {
          if (cancelled) break;
          if (evt.kind === 'session_artifacts') setEntries(evt.payload.artifacts);
        }
      } catch {
        if (!cancelled) setEntries([]);
      }
    })();
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [sessionId]);

  if (entries === null) {
    return (
      <div role="status" className="flex items-center gap-2 p-4 text-[13px] text-muted-foreground">
        <Loader2 className="size-4 animate-spin" aria-hidden />
        {t('common.loading')}
      </div>
    );
  }
  if (entries.length === 0) {
    return <p className="p-4 text-[13px] text-muted-foreground">{t('inspector.libraryEmpty')}</p>;
  }

  return (
    <>
      <ul className="p-2">
        {entries.map((entry) => (
          <li key={entry.path}>
            <button
              type="button"
              onClick={() => setOpened(entry.path)}
              className="flex h-8 w-full items-center gap-3 rounded-md px-2 text-left text-[13px] hover:bg-accent touch:h-11"
              title={entry.path}
            >
              <span className="min-w-0 flex-1 truncate">{entry.name}</span>
              <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                {formatBytes(entry.size)}
              </span>
            </button>
          </li>
        ))}
      </ul>
      <ArtifactsDrawer
        sessionId={sessionId}
        open={opened !== null}
        onOpenChange={(open) => !open && setOpened(null)}
        initialPath={opened}
      />
    </>
  );
}
