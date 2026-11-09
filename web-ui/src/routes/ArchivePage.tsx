import { Loader2 } from 'lucide-react';
import { useNavigate } from 'react-router-dom';
import { toast } from 'sonner';
import { SidebarToggle } from '@/components/shell/SidebarToggle';
import { unarchiveSession } from '@/lib/api/sessions';
import { useTranslation } from '@/lib/i18n';
import { profileSessionPath, useActiveProfile } from '@/lib/profile/active';
import { archivedSessions } from '@/lib/sessionLists';
import { useRuntimeStore } from '@/lib/stores/runtime';

/** The archived sessions of the active profile; opening one makes it recent again. */
export function ArchivePage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { profileId } = useActiveProfile();
  const profileSessions = useRuntimeStore((s) => s.profileSessions);
  const scoped = profileSessions && profileSessions.profile.id === profileId ? profileSessions : null;
  const archived = scoped ? archivedSessions(scoped) : [];

  const open = (sessionId: string) => {
    if (!profileId) return;
    unarchiveSession(sessionId)
      .then(() => navigate(profileSessionPath(profileId, sessionId)))
      .catch((err: unknown) =>
        toast.error(t('errors.generic'), {
          description: err instanceof Error ? err.message : String(err),
        }),
      );
  };

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-11 shrink-0 items-center px-2 touch:h-14 sm:px-3">
        <SidebarToggle />
      </header>
      <div className="mx-auto w-full max-w-2xl flex-1 overflow-y-auto px-4 pb-12 sm:px-6">
        <h1 className="text-lg font-semibold">{t('archive.title')}</h1>
        <p className="mt-1 text-[13px] text-muted-foreground">{t('archive.description')}</p>
        {scoped === null ? (
          <div role="status" className="flex items-center gap-2 py-10 text-[13px] text-muted-foreground">
            <Loader2 className="size-4 animate-spin" aria-hidden />
            {t('common.loading')}
          </div>
        ) : archived.length === 0 ? (
          <p className="py-10 text-[13px] text-muted-foreground">{t('archive.empty')}</p>
        ) : (
          <ul className="mt-4 divide-y divide-border rounded-lg border border-border">
            {archived.map((session) => (
              <li key={session.id}>
                <button
                  type="button"
                  onClick={() => open(session.id)}
                  className="flex min-h-9 w-full items-center gap-3 px-3 py-2 text-left text-[13px] hover:bg-accent touch:min-h-11"
                >
                  <span className="flex-1 truncate">{session.title || t('common.untitled')}</span>
                  {session.updated_at && (
                    <time
                      dateTime={session.updated_at}
                      className="shrink-0 text-xs tabular-nums text-muted-foreground"
                    >
                      {new Date(session.updated_at).toLocaleDateString()}
                    </time>
                  )}
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
