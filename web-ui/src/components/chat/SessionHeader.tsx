import { useEffect, useState } from 'react';
import { Pencil, Check, X, Folder } from 'lucide-react';
import type { ApiSession } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { useSessionsStore } from '@/lib/stores/sessions';
import { RuntimePhaseBadge } from '@/components/shell/RuntimePhaseDot';
import { useTranslation } from '@/lib/i18n';
import { ContextRing } from './ContextRing';
import { ArtifactsDrawer } from './ArtifactsDrawer';

export function SessionHeader({ session }: { session: ApiSession }) {
  const rename = useSessionsStore((s) => s.rename);
  const [editing, setEditing] = useState(false);
  const [title, setTitle] = useState(session.title);
  const [artifactsOpen, setArtifactsOpen] = useState(false);
  const { t } = useTranslation();

  useEffect(() => {
    setTitle(session.title);
  }, [session.title]);

  const submit = async () => {
    const next = title.trim();
    if (next && next !== session.title) await rename(session.id, next);
    setEditing(false);
  };

  return (
    <header className="flex items-center gap-3 border-b border-border bg-background/70 px-6 py-3 backdrop-blur">
      {editing ? (
        <>
          <input
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') submit();
              if (e.key === 'Escape') {
                setTitle(session.title);
                setEditing(false);
              }
            }}
            autoFocus
            className="flex-1 rounded-md border border-border bg-background px-2 py-1 text-sm outline-none focus:ring-2 focus:ring-ring"
          />
          <Button size="icon" variant="ghost" onClick={submit}>
            <Check className="size-4" />
          </Button>
          <Button
            size="icon"
            variant="ghost"
            onClick={() => {
              setTitle(session.title);
              setEditing(false);
            }}
          >
            <X className="size-4" />
          </Button>
        </>
      ) : (
        <>
          <h2 className="flex-1 truncate text-sm font-medium">
            {session.title || t('common.untitled')}
          </h2>
          <RuntimePhaseBadge state={session.runtime?.state} />
          <ContextRing sessionId={session.id} />
          <Button
            size="icon"
            variant="ghost"
            onClick={() => setArtifactsOpen(true)}
            aria-label={t('chat.artifacts')}
          >
            <Folder className="size-3.5" />
          </Button>
          <Button size="icon" variant="ghost" onClick={() => setEditing(true)} aria-label={t('common.rename')}>
            <Pencil className="size-3.5" />
          </Button>
          <ArtifactsDrawer
            sessionId={session.id}
            open={artifactsOpen}
            onOpenChange={setArtifactsOpen}
          />
        </>
      )}
    </header>
  );
}
