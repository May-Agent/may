import { useMemo, useState } from 'react';
import { Link, useLocation, useParams } from 'react-router-dom';
import {
  ChevronDown,
  ChevronRight,
  Github,
  Loader2,
  PanelLeftClose,
  Plus,
  Search,
  Settings as SettingsIcon,
  X,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { useTranslation } from '@/lib/i18n';
import { useUiStore } from '@/lib/stores/ui';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useActiveProfile } from '@/lib/profile/active';
import { useNewChat } from '@/lib/useNewChat';
import { shortcutAria, shortcutLabel } from '@/lib/shortcuts';
import { FolderNode, SessionNode, repoLabel } from './SessionNode';
import { NewChatRepoDialog } from './NewChatRepoDialog';
import { ProfileSwitcher } from './ProfileSwitcher';
import { NotificationBell } from './NotificationBell';
import type { ProfileSessions } from '@/lib/types';
import { cn } from '@/lib/utils';

/**
 * The source list: sessions grouped Pinned / Recent / Archived, with the
 * one primary action (New chat) on top. Shown as a column on wide screens
 * and as a sheet on narrow ones (`inSheet`).
 */
export function Sidebar({ inSheet = false }: { inSheet?: boolean }) {
  const { sessionId } = useParams();
  const location = useLocation();
  const setCommandOpen = useUiStore((s) => s.setCommandOpen);
  const toggleSidebar = useUiStore((s) => s.toggleSidebar);
  const setSidebarSheetOpen = useUiStore((s) => s.setSidebarSheetOpen);
  const profileSessions = useRuntimeStore((s) => s.profileSessions);
  const { profileId: activeProfileId } = useActiveProfile();
  const { start: startNewChat, creating, enabled: canCreate } = useNewChat();
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
  const loading = activeProfileId !== null && scoped === null;

  const [archiveOpen, setArchiveOpen] = useState(false);
  const [repoDialogOpen, setRepoDialogOpen] = useState(false);
  const settingsActive = location.pathname.startsWith('/settings');

  // GitHub repos already bound to sessions, offered as quick picks in the
  // "new chat in repo" dialog.
  const knownGithubRepos = useMemo(() => {
    const seen = new Set<string>();
    for (const item of [...pinned, ...regular, ...archived]) {
      const repo = item.type === 'session' ? item.repo : null;
      if (repo?.startsWith('github:')) seen.add(repo.slice(7));
    }
    return [...seen].sort();
  }, [pinned, regular, archived]);

  // Sessions bound to a repository cluster under a collapsible group
  // header; repo-less sessions stay flat above the groups.
  const [regularNoRepo, repoGroups] = useMemo(() => {
    const flat: typeof regular = [];
    const groups = new Map<string, typeof regular>();
    for (const item of regular) {
      if (item.repo) {
        const list = groups.get(item.repo) ?? [];
        list.push(item);
        groups.set(item.repo, list);
      } else {
        flat.push(item);
      }
    }
    const sorted = [...groups.entries()].sort(([a], [b]) =>
      repoLabel(a).localeCompare(repoLabel(b)),
    );
    return [flat, sorted] as const;
  }, [regular]);

  return (
    <aside
      aria-label={t('sidebar.sessions')}
      className={cn(
        'flex h-full shrink-0 flex-col bg-sidebar text-sidebar-foreground',
        inSheet ? 'w-full' : 'w-64 border-r border-border',
      )}
    >
      <div className="flex h-11 items-center gap-0.5 px-3 touch:h-14">
        <Link
          to="/"
          className="mr-auto rounded-md px-1 text-[13px] font-semibold"
        >
          {t('appName')}
        </Link>
        <Button
          size="icon"
          variant="ghost"
          onClick={() => setCommandOpen(true)}
          aria-label={t('common.search')}
          aria-keyshortcuts={shortcutAria('search')}
          title={`${t('common.search')} (${shortcutLabel('search')})`}
        >
          <Search />
        </Button>
        <NotificationBell />
        {inSheet ? (
          <Button
            size="icon"
            variant="ghost"
            onClick={() => setSidebarSheetOpen(false)}
            aria-label={t('common.close')}
          >
            <X />
          </Button>
        ) : (
          <Button
            size="icon"
            variant="ghost"
            onClick={toggleSidebar}
            aria-label={t('shell.hideSidebar')}
            aria-keyshortcuts={shortcutAria('toggleSidebar')}
            title={`${t('shell.hideSidebar')} (${shortcutLabel('toggleSidebar')})`}
          >
            <PanelLeftClose />
          </Button>
        )}
      </div>

      <div className="flex gap-1 px-3 pb-2">
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              variant="outline"
              className="w-full justify-start gap-2 px-2 font-medium"
              onClick={() => void startNewChat()}
              disabled={!canCreate || creating}
              aria-busy={creating}
              aria-keyshortcuts={shortcutAria('newChat')}
            >
              {creating ? <Loader2 className="animate-spin" aria-hidden /> : <Plus aria-hidden />}
              <span className="flex-1 text-left">{t('sidebar.newChat')}</span>
              <kbd className="font-sans text-xs font-normal text-muted-foreground touch:hidden">
                {shortcutLabel('newChat')}
              </kbd>
            </Button>
          </TooltipTrigger>
          <TooltipContent side="right">
            {t('shell.newChatShortcut', { shortcut: shortcutLabel('newChat') })}
          </TooltipContent>
        </Tooltip>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              variant="outline"
              size="icon"
              className="shrink-0"
              onClick={() => setRepoDialogOpen(true)}
              disabled={!canCreate}
              aria-label={t('sidebar.newChatRepo.title')}
            >
              <Github aria-hidden />
            </Button>
          </TooltipTrigger>
          <TooltipContent side="right">{t('sidebar.newChatRepo.title')}</TooltipContent>
        </Tooltip>
      </div>
      <NewChatRepoDialog
        open={repoDialogOpen}
        onOpenChange={setRepoDialogOpen}
        knownRepos={knownGithubRepos}
        busy={creating}
        onSubmit={(repo) => {
          setRepoDialogOpen(false);
          void startNewChat({ repo: `github:${repo}` });
        }}
      />

      <ScrollArea className="flex-1 px-2 pb-2">
        <nav aria-label={t('sidebar.sessions')}>
          {loading ? (
            <div
              role="status"
              className="flex items-center justify-center gap-2 px-2 py-10 text-[13px] text-muted-foreground"
            >
              <Loader2 className="size-4 animate-spin" aria-hidden />
              {t('common.loading')}
            </div>
          ) : totalActive === 0 && archived.length === 0 ? (
            <div className="px-3 py-10 text-center text-[13px] text-muted-foreground">
              {t('sidebar.noSessionsInProfile')}
            </div>
          ) : (
            <div className="space-y-3">
              {pinned.length > 0 && (
                <Section label={t('sidebar.pinned')}>
                  {pinned.map((item) =>
                    item.type === 'session' ? (
                      <SessionNode
                        key={item.id}
                        id={item.id}
                        title={item.title || t('common.untitled')}
                        runtime={item.runtime}
                        kind="pinned"
                        active={item.id === sessionId}
                        repo={item.repo}
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
                            repo={child.repo}
                          />
                        ))}
                      </FolderNode>
                    ),
                  )}
                </Section>
              )}

              {regular.length > 0 && (
                <Section label={t('sidebar.recent')}>
                  {regularNoRepo.map((item) => (
                    <SessionNode
                      key={item.id}
                      id={item.id}
                      title={item.title || t('common.untitled')}
                      runtime={item.runtime}
                      kind="regular"
                      active={item.id === sessionId}
                      repo={item.repo}
                    />
                  ))}
                  {repoGroups.map(([repo, items]) => (
                    <RepoGroup key={repo} repo={repo} count={items.length}>
                      {items.map((item) => (
                        <SessionNode
                          key={item.id}
                          id={item.id}
                          title={item.title || t('common.untitled')}
                          runtime={item.runtime}
                          kind="regular"
                          active={item.id === sessionId}
                          indent
                        />
                      ))}
                    </RepoGroup>
                  ))}
                </Section>
              )}

              {archived.length > 0 && (
                <div>
                  <button
                    type="button"
                    onClick={() => setArchiveOpen((v) => !v)}
                    className="flex h-7 w-full items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground hover:text-foreground touch:h-11"
                    aria-expanded={archiveOpen}
                  >
                    {archiveOpen ? (
                      <ChevronDown className="size-3.5" aria-hidden />
                    ) : (
                      <ChevronRight className="size-3.5" aria-hidden />
                    )}
                    <span>{t('sidebar.archived')}</span>
                    <span className="ml-auto font-normal tabular-nums">{archived.length}</span>
                  </button>
                  {archiveOpen && (
                    <ul className="mt-0.5 space-y-px">
                      {archived.map((item) => (
                        <SessionNode
                          key={item.id}
                          id={item.id}
                          title={item.title || t('common.untitled')}
                          runtime={item.runtime}
                          kind="archived"
                          active={item.id === sessionId}
                          repo={item.repo}
                        />
                      ))}
                    </ul>
                  )}
                </div>
              )}
            </div>
          )}
        </nav>
      </ScrollArea>

      <div className="px-2 pt-1">
        <Link
          to="/settings"
          aria-current={settingsActive ? 'page' : undefined}
          className={cn(
            'flex h-7 items-center gap-2 rounded-md px-2 text-[13px] font-medium text-muted-foreground hover:bg-accent hover:text-foreground touch:h-11',
            settingsActive && 'bg-accent text-foreground',
          )}
        >
          <SettingsIcon className="size-4" strokeWidth={1.75} aria-hidden />
          {t('sidebar.settings')}
        </Link>
      </div>

      <ProfileSwitcher />
    </aside>
  );
}

interface SectionProps {
  label: string;
  children: React.ReactNode;
}

function Section({ label, children }: SectionProps) {
  return (
    <section aria-label={label}>
      <h3 className="flex h-7 items-center px-2 text-xs font-medium text-muted-foreground">{label}</h3>
      <ul className="space-y-px">{children}</ul>
    </section>
  );
}

interface RepoGroupProps {
  repo: string;
  count: number;
  children: React.ReactNode;
}

/** Collapsible header binding the sessions of one repository together. */
function RepoGroup({ repo, count, children }: RepoGroupProps) {
  const [open, setOpen] = useState(true);
  return (
    <li>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex h-7 w-full items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground hover:text-foreground touch:h-11"
        aria-expanded={open}
      >
        {open ? (
          <ChevronDown className="size-3.5" aria-hidden />
        ) : (
          <ChevronRight className="size-3.5" aria-hidden />
        )}
        <span className="truncate" title={repo}>
          {repoLabel(repo)}
        </span>
        <span className="ml-auto font-normal tabular-nums">{count}</span>
      </button>
      {open && <ul className="space-y-px">{children}</ul>}
    </li>
  );
}
