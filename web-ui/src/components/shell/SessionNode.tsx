import { useState, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import * as DropdownMenu from '@radix-ui/react-dropdown-menu';
import {
  Archive,
  ArchiveRestore,
  ChevronDown,
  ChevronRight,
  Folder,
  FolderMinus,
  MessageSquare,
  MoreHorizontal,
  Pencil,
  Pin,
  PinOff,
  Trash2,
} from 'lucide-react';
import { toast } from 'sonner';
import { useTranslation } from '@/lib/i18n';
import {
  archiveSession as apiArchive,
  pinSession,
  unarchiveSession as apiUnarchive,
  unpinSession,
  moveSessionToFolder,
} from '@/lib/api/sessions';
import { useSessionsStore } from '@/lib/stores/sessions';
import { cn } from '@/lib/utils';
import type { SessionRuntimeSummary } from '@/lib/types';
import { ConfirmDialog } from '@/components/ui/confirm-dialog';
import { RenameDialog } from '@/components/ui/rename-dialog';
import { RuntimePhaseDot } from './RuntimePhaseDot';
import { profilePath, profileSessionPath, useActiveProfile } from '@/lib/profile/active';

export type SessionNodeKind = 'pinned' | 'regular' | 'archived' | 'folder-child';

interface SessionNodeProps {
  id: string;
  title: string;
  runtime?: SessionRuntimeSummary;
  kind: SessionNodeKind;
  active: boolean;
  inFolder?: boolean;
  indent?: boolean;
}

/** One source-list row: the chat's title, its state, and an actions menu. */
export function SessionNode({
  id,
  title,
  runtime,
  kind,
  active,
  inFolder,
  indent,
}: SessionNodeProps) {
  const { profileId } = useActiveProfile();
  const href = profileId ? profileSessionPath(profileId, id) : `/s/${id}`;
  return (
    <li className="group relative">
      <Link
        to={href}
        aria-current={active ? 'page' : undefined}
        className={cn(
          'flex h-8 items-center gap-2 rounded-md pl-2 pr-9 text-[13px] transition-colors hover:bg-accent touch:h-11 touch:pr-12',
          active && 'bg-accent font-medium',
          indent && 'pl-7',
        )}
      >
        <MessageSquare className="size-4 shrink-0 text-muted-foreground" aria-hidden />
        <span className="flex-1 truncate">{title}</span>
        <RuntimePhaseDot state={runtime?.state} />
      </Link>
      <SessionNodeMenu id={id} title={title} kind={kind} inFolder={inFolder ?? false} active={active} />
    </li>
  );
}

interface FolderNodeProps {
  id: string;
  name: string;
  children: ReactNode;
  defaultOpen?: boolean;
}

export function FolderNode({ id, name, children, defaultOpen = false }: FolderNodeProps) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <li>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex h-8 w-full items-center gap-1.5 rounded-md px-2 text-[13px] text-muted-foreground hover:bg-accent touch:h-11"
        aria-expanded={open}
        aria-controls={`folder-${id}`}
      >
        {open ? (
          <ChevronDown className="size-3.5" aria-hidden />
        ) : (
          <ChevronRight className="size-3.5" aria-hidden />
        )}
        <Folder className="size-4" aria-hidden />
        <span className="truncate">{name}</span>
      </button>
      {open && (
        <ul id={`folder-${id}`} className="space-y-px">
          {children}
        </ul>
      )}
    </li>
  );
}

interface SessionNodeMenuProps {
  id: string;
  title: string;
  kind: SessionNodeKind;
  inFolder: boolean;
  active: boolean;
}

function SessionNodeMenu({ id, title, kind, inFolder, active }: SessionNodeMenuProps) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { profileId } = useActiveProfile();
  const renameSession = useSessionsStore((s) => s.rename);
  const removeSession = useSessionsStore((s) => s.remove);
  const [open, setOpen] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const run = (fn: () => Promise<void>) => () => {
    void fn().catch((err: unknown) =>
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : String(err),
      }),
    );
  };

  const reportFailure = async (fn: () => Promise<void>) => {
    try {
      await fn();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : String(err),
      });
      throw err;
    }
  };

  return (
    <>
      <DropdownMenu.Root open={open} onOpenChange={setOpen}>
        <DropdownMenu.Trigger asChild>
          <button
            type="button"
            aria-label={t('shell.sessionActions')}
            title={t('shell.sessionActions')}
            className={cn(
              'absolute right-1 top-1/2 flex size-7 -translate-y-1/2 items-center justify-center rounded-md text-muted-foreground',
              'hover:bg-background hover:text-foreground focus-visible:opacity-100',
              'opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 touch:size-11 touch:opacity-100',
              (open || active) && 'opacity-100',
            )}
          >
            <MoreHorizontal className="size-4" aria-hidden />
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content
            align="end"
            sideOffset={4}
            className="z-50 min-w-48 rounded-lg border border-border bg-popover p-1 text-popover-foreground shadow-lg data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95"
          >
            <MenuItem
              icon={kind === 'pinned' ? PinOff : Pin}
              label={kind === 'pinned' ? t('sidebar.unpin') : t('sidebar.pin')}
              onSelect={run(() => (kind === 'pinned' ? unpinSession(id) : pinSession(id)))}
              disabled={kind === 'archived'}
            />
            {inFolder && (
              <MenuItem
                icon={FolderMinus}
                label={t('sidebar.removeFromFolder')}
                onSelect={run(() => moveSessionToFolder(id, null))}
              />
            )}
            <MenuItem
              icon={Pencil}
              label={`${t('sidebar.rename')}…`}
              onSelect={() => setRenaming(true)}
            />
            <MenuItem
              icon={kind === 'archived' ? ArchiveRestore : Archive}
              label={kind === 'archived' ? t('sidebar.unarchiveSession') : t('sidebar.archiveSession')}
              onSelect={run(() => (kind === 'archived' ? apiUnarchive(id) : apiArchive(id)))}
            />
            <DropdownMenu.Separator className="my-1 h-px bg-border" />
            <MenuItem
              icon={Trash2}
              label={`${t('sidebar.delete')}…`}
              onSelect={() => setDeleting(true)}
              destructive
            />
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>

      <RenameDialog
        open={renaming}
        onOpenChange={setRenaming}
        title={t('shell.renameTitle')}
        label={t('shell.renameLabel')}
        initialValue={title}
        onSubmit={(next) => reportFailure(() => renameSession(id, next))}
      />
      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={t('shell.deleteTitle', { title })}
        description={t('shell.deleteBody')}
        confirmLabel={t('shell.deleteConfirm')}
        destructive
        onConfirm={() =>
          reportFailure(async () => {
            await removeSession(id);
            if (active) navigate(profileId ? profilePath(profileId) : '/');
          })
        }
      />
    </>
  );
}

interface MenuItemProps {
  icon: typeof Pin;
  label: string;
  onSelect: () => void;
  disabled?: boolean;
  destructive?: boolean;
}

function MenuItem({ icon: Icon, label, onSelect, disabled, destructive }: MenuItemProps) {
  return (
    <DropdownMenu.Item
      onSelect={onSelect}
      disabled={disabled}
      className={cn(
        'flex h-8 cursor-default select-none items-center gap-2 rounded-md px-2 text-[13px] outline-none',
        'data-[highlighted]:bg-accent data-[disabled]:pointer-events-none data-[disabled]:opacity-50 touch:h-11',
        destructive && 'text-destructive data-[highlighted]:bg-destructive/10',
      )}
    >
      <Icon className="size-4" aria-hidden />
      {label}
    </DropdownMenu.Item>
  );
}
