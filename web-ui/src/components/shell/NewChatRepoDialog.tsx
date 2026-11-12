import { useEffect, useState } from 'react';
import { Github } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { listGithubRepos } from '@/lib/api/githubRepos';
import { ApiError } from '@/lib/api/client';
import { useTranslation } from '@/lib/i18n';
import { RepoPickerList, type RepoListState } from './RepoPickerList';

interface NewChatRepoDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the picked `owner/repo`; the caller starts the session. */
  onPick: (repo: string) => void;
  busy?: boolean;
}

/**
 * Start a chat bound to a GitHub repository that needs no local checkout —
 * the session binds to `github:owner/repo` and the cloud remote is the
 * source of truth. The repository is picked from the daemon's list of what
 * the host's GitHub account can reach.
 */
export function NewChatRepoDialog({
  open,
  onOpenChange,
  onPick,
  busy = false,
}: NewChatRepoDialogProps) {
  const { t } = useTranslation();
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="gap-3 sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Github className="size-4" aria-hidden />
            {t('sidebar.newChatRepo.title')}
          </DialogTitle>
          <DialogDescription>{t('sidebar.newChatRepo.description')}</DialogDescription>
        </DialogHeader>
        <RepoPicker onPick={onPick} busy={busy} />
      </DialogContent>
    </Dialog>
  );
}

/** Mounted with the dialog content, so each opening fetches a fresh list. */
function RepoPicker({ onPick, busy }: { onPick: (repo: string) => void; busy: boolean }) {
  const { t } = useTranslation();
  const [query, setQuery] = useState('');
  const [state, setState] = useState<RepoListState>({ status: 'loading' });

  useEffect(() => {
    let cancelled = false;
    listGithubRepos().then(
      ({ repos }) => {
        if (!cancelled) setState({ status: 'ready', repos });
      },
      (error: unknown) => {
        if (cancelled) return;
        if (error instanceof ApiError && error.code === 'github_unauthenticated') {
          setState({ status: 'unauthenticated', message: t('sidebar.newChatRepo.unauthenticated') });
        } else {
          setState({
            status: 'error',
            message: error instanceof Error ? error.message : t('errors.generic'),
          });
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, [t]);

  return (
    <div className="overflow-hidden rounded-md border border-border">
      <RepoPickerList
        state={state}
        query={query}
        onQueryChange={setQuery}
        onPick={onPick}
        busy={busy}
        labels={{
          search: t('sidebar.newChatRepo.search'),
          loading: t('common.loading'),
          empty: t('sidebar.newChatRepo.empty'),
          recent: t('sidebar.newChatRepo.recent'),
          all: t('sidebar.newChatRepo.all'),
        }}
      />
    </div>
  );
}
