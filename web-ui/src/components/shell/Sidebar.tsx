import { useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { ChevronDown, ChevronRight, Pin, Plus, Search, Settings as SettingsIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTranslation } from '@/lib/i18n';
import { useUiStore } from '@/lib/stores/ui';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useActiveProfile, profileSessionPath } from '@/lib/profile/active';
import { FolderNode, SessionNode } from './SessionNode';
import { ProfileSwitcher } from './ProfileSwitcher';
import { NotificationBell } from './NotificationBell';
import type { ProfileSessions } from '@/lib/types';
import { cn } from '@/lib/utils';

export function Sidebar() {
  const navigate = useNavigate();
  const { sessionId } = useParams();
  const createNew = useSessionsStore((s) => s.createNew);
  const setCommandOpen = useUiStore((s) => s.setCommandOpen);
  const profileSessions = useRuntimeStore((s) => s.profileSessions);
  const { profileId: activeProfileId } = useActiveProfile();
  const { t } = useTranslation();

  const scoped: ProfileSessions | null = useMemo(() => {
    if (!profileSessions) return null;
    if (activeProfileId && profileSessions.profile.id !== activeProfileId) return null;
    return profileSessions;
  }, [profileSessions, activeProfileId]);

  const pinned = scoped?.pinned ?? [];
  const regular = scoped?.regular ?? [];
  const archived = scoped?.archived ?? [];
  const totalActive = pinned.length + regular.length;

  const [archiveOpen, setArchiveOpen] = useState(false);

  const handleCreate = async () => {
    if (!activeProfileId) return;
    const s = await createNew(activeProfileId);
    navigate(profileSessionPath(activeProfileId, s.id));
  };

  return (
    <aside className="flex h-full w-64 flex-col border-r border-border bg-card/40">
      <div className="flex items-center justify-between px-3 py-2.5">
        <Link to="/" className="text-sm font-semibold tracking-tight">
          {t('appName')}
        </Link>
        <div className="flex gap-0.5">
          <Button
            size="icon"
            variant="ghost"
            onClick={() => setCommandOpen(true)}
            aria-label={t('common.search')}
          >
            <Search className="size-4" />
          </Button>
          <NotificationBell />
          <Button size="icon" variant="ghost" onClick={handleCreate} aria-label={t('sidebar.newChat')}>
            <Plus className="size-4" />
          </Button>
        </div>
      </div>

      <ScrollArea className="flex-1 px-2 pb-2">
        {totalActive === 0 && archived.length === 0 ? (
          <div className="px-2 py-10 text-center text-xs text-muted-foreground">
            {t('sidebar.noSessionsInProfile')}
          </div>
        ) : (
          <div className="space-y-3">
            {pinned.length > 0 && (
              <Section label={t('sidebar.pinned')} icon={<Pin className="size-3" />}>
                {pinned.map((item) =>
                  item.type === 'session' ? (
                    <SessionNode
                      key={item.id}
                      id={item.id}
                      title={item.title || t('common.untitled')}
                      runtime={item.runtime}
                      kind="pinned"
                      active={item.id === sessionId}
                    />
                  ) : (
                    <FolderNode key={item.id} id={item.id} name={item.name} defaultOpen>
                      {item.sessions.map((child) => (
                        <SessionNode
                          key={child.id}
                          id={child.id}
                          title={child.title || t('common.untitled')}
                          runtime={child.runtime}
                          kind="folder-child"
                          active={child.id === sessionId}
                          inFolder
                          indent
                        />
                      ))}
                    </FolderNode>
                  ),
                )}
              </Section>
            )}

            {regular.length > 0 && (
              <Section label={t('sidebar.recent')}>
                {regular.map((item) => (
                  <SessionNode
                    key={item.id}
                    id={item.id}
                    title={item.title || t('common.untitled')}
                    runtime={item.runtime}
                    kind="regular"
                    active={item.id === sessionId}
                  />
                ))}
              </Section>
            )}

            {archived.length > 0 && (
              <div>
                <button
                  type="button"
                  onClick={() => setArchiveOpen((v) => !v)}
                  className="flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground hover:text-foreground"
                  aria-expanded={archiveOpen}
                >
                  {archiveOpen ? (
                    <ChevronDown className="size-3" />
                  ) : (
                    <ChevronRight className="size-3" />
                  )}
                  <span>{t('sidebar.archived')}</span>
                  <span className="opacity-60">· {archived.length}</span>
                </button>
                {archiveOpen && (
                  <div className="mt-1 space-y-0.5">
                    {archived.map((item) => (
                      <SessionNode
                        key={item.id}
                        id={item.id}
                        title={item.title || t('common.untitled')}
                        runtime={item.runtime}
                        kind="archived"
                        active={item.id === sessionId}
                      />
                    ))}
                  </div>
                )}
              </div>
            )}
          </div>
        )}
      </ScrollArea>

      <Link
        to="/settings"
        className={cn(
          'mx-2 mb-1 flex items-center gap-2 rounded-md px-2.5 py-2 text-sm hover:bg-accent',
        )}
      >
        <SettingsIcon className="size-4 text-muted-foreground" />
        {t('sidebar.settings')}
      </Link>

      <ProfileSwitcher />
    </aside>
  );
}

interface SectionProps {
  label: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}

function Section({ label, icon, children }: SectionProps) {
  return (
    <div>
      <div className="flex items-center gap-1.5 px-2 pb-1 text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
        {icon}
        <span>{label}</span>
      </div>
      <div className="space-y-0.5">{children}</div>
    </div>
  );
}
