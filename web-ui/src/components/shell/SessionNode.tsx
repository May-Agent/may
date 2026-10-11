import { useState, type MouseEvent, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import {
  Archive,
  ArchiveRestore,
  ChevronDown,
  ChevronRight,
  Folder,
  MessageSquare,
  MoreHorizontal,
  Pin,
  PinOff,
  Trash2,
} from 'lucide-react';
import { toast } from 'sonner';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
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

export function SessionNode({
  id,
  title,
  runtime,
  kind,
  active,
  inFolder,
  indent,
}: SessionNodeProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const { profileId } = useActiveProfile();
  const href = profileId ? profileSessionPath(profileId, id) : `/s/${id}`;
  return (
    <Link
      to={href}
      className={cn(
        'group relative flex items-center gap-2 rounded-md px-2 py-1.5 text-[13px] transition-colors hover:bg-accent',
        active && 'bg-accent',
        indent && 'pl-6',
      )}
    >
      <MessageSquare className="size-3.5 shrink-0 text-muted-foreground" />
      <span className="truncate flex-1">{title}</span>
      <RuntimePhaseDot state={runtime?.state} />
      <SessionNodeMenu
        id={id}
        kind={kind}
        inFolder={inFolder ?? false}
        open={menuOpen}
        onOpenChange={setMenuOpen}
      />
    </Link>
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
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-1.5 rounded-md px-2 py-1.5 text-[13px] text-muted-foreground hover:bg-accent"
        aria-expanded={open}
        aria-controls={`folder-${id}`}
      >
        {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
        <Folder className="size-3.5" />
        <span className="truncate">{name}</span>
      </button>
      {open && (
        <div id={`folder-${id}`} className="space-y-0.5">
          {children}
        </div>
      )}
    </div>
  );
}

interface SessionNodeMenuProps {
  id: string;
  kind: SessionNodeKind;
  inFolder: boolean;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

function SessionNodeMenu({ id, kind, inFolder, open, onOpenChange }: SessionNodeMenuProps) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { profileId } = useActiveProfile();
  const renameSession = useSessionsStore((s) => s.rename);
  const removeSession = useSessionsStore((s) => s.remove);
  const [busy, setBusy] = useState(false);

  const stopNav = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
  };

  const guarded = async (fn: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    try {
      await fn();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
      onOpenChange(false);
    }
  };

  const handlePin = () => guarded(() => pinSession(id));
  const handleUnpin = () => guarded(() => unpinSession(id));
  const handleArchive = () => guarded(() => apiArchive(id));
  const handleUnarchive = () => guarded(() => apiUnarchive(id));
  const handleRemoveFromFolder = () => guarded(() => moveSessionToFolder(id, null));

  const handleRename = () =>
    guarded(async () => {
      const next = window.prompt(t('sidebar.rename'));
      if (next && next.trim()) await renameSession(id, next.trim());
    });

  const handleDelete = () =>
    guarded(async () => {
      if (!window.confirm(`${t('sidebar.delete')}?`)) return;
      await removeSession(id);
      navigate(profileId ? profilePath(profileId) : '/');
    });

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger
        asChild
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          onOpenChange(!open);
        }}
      >
        <button
          type="button"
          aria-label={t('sidebar.actions')}
          className={cn(
            'flex size-5 shrink-0 items-center justify-center rounded text-muted-foreground opacity-0 hover:bg-background hover:text-foreground group-hover:opacity-100',
            open && 'opacity-100',
          )}
        >
          <MoreHorizontal className="size-3.5" />
        </button>
      </PopoverTrigger>
      <PopoverContent
        className="w-48 p-1"
        align="end"
        onClick={stopNav}
      >
        <MenuItem
          icon={kind === 'pinned' ? PinOff : Pin}
          label={kind === 'pinned' ? t('sidebar.unpin') : t('sidebar.pin')}
          onClick={kind === 'pinned' ? handleUnpin : handlePin}
          disabled={busy || kind === 'archived'}
        />
        {inFolder && (
          <MenuItem
            icon={Folder}
            label={t('sidebar.removeFromFolder')}
            onClick={handleRemoveFromFolder}
            disabled={busy}
          />
        )}
        <MenuItem
          icon={kind === 'archived' ? ArchiveRestore : Archive}
          label={kind === 'archived' ? t('sidebar.unarchiveSession') : t('sidebar.archiveSession')}
          onClick={kind === 'archived' ? handleUnarchive : handleArchive}
          disabled={busy}
        />
        <MenuItem
          icon={MessageSquare}
          label={t('sidebar.rename')}
          onClick={handleRename}
          disabled={busy}
        />
        <div className="my-1 h-px bg-border" />
        <MenuItem
          icon={Trash2}
          label={t('sidebar.delete')}
          onClick={handleDelete}
          disabled={busy}
          destructive
        />
      </PopoverContent>
    </Popover>
  );
}

interface MenuItemProps {
  icon: typeof Pin;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  destructive?: boolean;
}

function MenuItem({ icon: Icon, label, onClick, disabled, destructive }: MenuItemProps) {
  return (
    <button
      type="button"
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }}
      disabled={disabled}
      className={cn(
        'flex w-full items-center gap-2 rounded px-2 py-1.5 text-sm hover:bg-accent',
        destructive && 'text-destructive hover:bg-destructive/10',
        disabled && 'opacity-50 cursor-not-allowed',
      )}
    >
      <Icon className="size-3.5" />
      {label}
    </button>
  );
}
