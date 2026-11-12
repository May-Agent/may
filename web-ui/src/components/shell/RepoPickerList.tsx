import { Github, Loader2 } from 'lucide-react';
import { Command } from 'cmdk';
import type { GithubRepo } from '@/lib/api/githubRepos';
import { filterRepos } from '@/lib/repo';

export type RepoListState =
  | { status: 'loading' }
  | { status: 'ready'; repos: GithubRepo[] }
  /** The host has no GitHub login. */
  | { status: 'unauthenticated'; message: string }
  | { status: 'error'; message: string };

export interface RepoPickerLabels {
  search: string;
  loading: string;
  empty: string;
  recent: string;
  all: string;
}

interface RepoPickerListProps {
  state: RepoListState;
  query: string;
  onQueryChange: (query: string) => void;
  /** Called with `owner/repo` of the picked entry. */
  onPick: (fullName: string) => void;
  busy?: boolean;
  labels: RepoPickerLabels;
}

/**
 * Search box over the repositories a chat can start on. Filtering is ours
 * (`shouldFilter={false}`) so the shown entries are exactly `filterRepos`,
 * and a session starts only from an entry of the list.
 */
export function RepoPickerList({
  state,
  query,
  onQueryChange,
  onPick,
  busy = false,
  labels,
}: RepoPickerListProps) {
  const repos = state.status === 'ready' ? filterRepos(state.repos, query) : [];
  const recent = repos.filter((repo) => repo.recent);
  const others = repos.filter((repo) => !repo.recent);

  const group = (heading: string, entries: GithubRepo[]) =>
    entries.length > 0 && (
      <Command.Group
        heading={heading}
        className="text-[11px] uppercase tracking-wider text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2"
      >
        {entries.map((repo) => (
          <Command.Item
            key={repo.fullName}
            value={repo.fullName}
            disabled={busy}
            onSelect={() => onPick(repo.fullName)}
            className="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-[13px] normal-case tracking-normal text-foreground aria-selected:bg-accent data-[disabled=true]:opacity-50"
          >
            <Github className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
            <span className="truncate">{repo.fullName}</span>
          </Command.Item>
        ))}
      </Command.Group>
    );

  return (
    <Command shouldFilter={false} loop className="flex flex-col overflow-hidden">
      <Command.Input
        value={query}
        onValueChange={onQueryChange}
        placeholder={labels.search}
        autoFocus
        className="w-full border-b border-border bg-transparent px-3 py-2 text-sm outline-none placeholder:text-muted-foreground"
      />
      <Command.List className="max-h-[320px] overflow-y-auto p-1 scrollbar-thin">
        {state.status === 'loading' && (
          <div
            role="status"
            className="flex items-center justify-center gap-2 px-3 py-6 text-xs text-muted-foreground"
          >
            <Loader2 className="size-4 animate-spin" aria-hidden />
            {labels.loading}
          </div>
        )}
        {(state.status === 'unauthenticated' || state.status === 'error') && (
          <div role="alert" className="px-3 py-6 text-center text-xs text-muted-foreground">
            {state.message}
          </div>
        )}
        {state.status === 'ready' && repos.length === 0 && (
          <div className="px-3 py-6 text-center text-xs text-muted-foreground">{labels.empty}</div>
        )}
        {group(labels.recent, recent)}
        {group(labels.all, others)}
      </Command.List>
    </Command>
  );
}
