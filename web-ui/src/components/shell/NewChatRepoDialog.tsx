import { useState } from 'react';
import { Github, Loader2 } from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useTranslation } from '@/lib/i18n';

interface NewChatRepoDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Repositories other sessions are already bound to, `owner/repo` form. */
  knownRepos: string[];
  /** Called with `owner/repo`; the caller starts the session. */
  onSubmit: (repo: string) => void;
  busy?: boolean;
}

const REPO_RE = /^[\w.-]+\/[\w.-]+$/;

/**
 * Start a chat bound to a GitHub repository that needs no local checkout —
 * the session binds to `github:owner/repo` and the cloud remote is the
 * source of truth.
 */
export function NewChatRepoDialog({
  open,
  onOpenChange,
  knownRepos,
  onSubmit,
  busy = false,
}: NewChatRepoDialogProps) {
  const { t } = useTranslation();
  const [value, setValue] = useState('');
  const trimmed = value.trim();
  const valid = REPO_RE.test(trimmed);

  const submit = (repo: string) => {
    onSubmit(repo);
    setValue('');
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Github className="size-4" aria-hidden />
            {t('sidebar.newChatRepo.title')}
          </DialogTitle>
          <DialogDescription>{t('sidebar.newChatRepo.description')}</DialogDescription>
        </DialogHeader>
        <form
          className="flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid && !busy) submit(trimmed);
          }}
        >
          <Input
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={t('sidebar.newChatRepo.placeholder')}
            aria-label={t('sidebar.newChatRepo.placeholder')}
            autoFocus
          />
          <Button type="submit" disabled={!valid || busy}>
            {busy ? <Loader2 className="animate-spin" aria-hidden /> : t('sidebar.newChatRepo.start')}
          </Button>
        </form>
        {knownRepos.length > 0 && (
          <div className="mt-1">
            <div className="px-1 pb-1 text-xs font-medium text-muted-foreground">
              {t('sidebar.newChatRepo.known')}
            </div>
            <ul className="max-h-48 space-y-px overflow-y-auto">
              {knownRepos.map((repo) => (
                <li key={repo}>
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => submit(repo)}
                    className="flex h-8 w-full items-center gap-2 rounded-md px-2 text-[13px] hover:bg-accent disabled:opacity-50"
                  >
                    <Github className="size-3.5 text-muted-foreground" aria-hidden />
                    <span className="truncate">{repo}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
