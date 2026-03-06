import { useState, useCallback, useMemo } from 'react';
import { isToday, isYesterday, isAfter, subDays, startOfDay } from 'date-fns';
import { useStore } from '@/lib/store';
import { useRuntimeStore } from '@/lib/runtimeStore';
import { cn } from '@/lib/utils';
import { resolveWorkspaceSurfaceState } from '@/lib/containerPrepare';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Separator } from '@/components/ui/separator';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  Plus,
  Trash2,
  Settings,
  FolderPlus,
  FolderOpen,
  ChevronDown,
  ChevronRight,
  MoreHorizontal,
  FolderClosed,
  Sun,
  Briefcase,
  Monitor,
  Box,
  Laptop,
  type LucideIcon,
} from 'lucide-react';
import {
  DndContext,
  DragOverlay,
  useDraggable,
  useDroppable,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragStartEvent,
} from '@dnd-kit/core';
import type { SessionSummary, PinnedItem, ProfileInfo } from '@/types';

interface SidebarProps {
  className?: string;
  onOpenSettings: () => void;
  onSelectSession?: (sessionId: string) => void;
  onNewChat?: () => void;
}

// ── Time grouping helpers ────────────────────────────────

type TimeGroup = 'Today' | 'Yesterday' | 'Previous 7 Days' | 'Older';

function getTimeGroup(dateStr: string | null): TimeGroup {
  if (!dateStr) return 'Older';
  const date = new Date(dateStr);
  if (isToday(date)) return 'Today';
  if (isYesterday(date)) return 'Yesterday';
  if (isAfter(date, subDays(startOfDay(new Date()), 7))) return 'Previous 7 Days';
  return 'Older';
}

function groupByTime(sessions: SessionSummary[]): Map<TimeGroup, SessionSummary[]> {
  const groups = new Map<TimeGroup, SessionSummary[]>();
  for (const s of sessions) {
    const group = getTimeGroup(s.updatedAt);
    const list = groups.get(group) ?? [];
    list.push(s);
    groups.set(group, list);
  }
  return groups;
}

const TIME_GROUP_ORDER: TimeGroup[] = ['Today', 'Yesterday', 'Previous 7 Days', 'Older'];

// ── Profile icon mapping ─────────────────────────────────

const PROFILE_COLORS = [
  'bg-rose-500', 'bg-sky-500', 'bg-amber-500', 'bg-emerald-500',
  'bg-violet-500', 'bg-orange-500', 'bg-teal-500', 'bg-pink-500',
];

const PROFILE_RING_COLORS = [
  'ring-rose-400/40', 'ring-sky-400/40', 'ring-amber-400/40', 'ring-emerald-400/40',
  'ring-violet-400/40', 'ring-orange-400/40', 'ring-teal-400/40', 'ring-pink-400/40',
];

const SLUG_ICON: Record<string, LucideIcon> = {
  daily: Sun,
  work: Briefcase,
  local: Monitor,
};

const ENV_ICON: Record<string, LucideIcon> = {
  container: Box,
  local: Laptop,
};

function getProfileIcon(profile: ProfileInfo): LucideIcon {
  if (profile.slug) {
    const mapped = SLUG_ICON[profile.slug];
    if (mapped) return mapped;
  }
  return ENV_ICON[profile.environment.type] ?? Laptop;
}

function profileColor(index: number): string {
  return PROFILE_COLORS[index % PROFILE_COLORS.length];
}

// ── Streaming indicator ──────────────────────────────────

function StreamingDots() {
  return (
    <span className="flex gap-0.5" aria-label="running session">
      <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse" />
      <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse [animation-delay:150ms]" />
      <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse [animation-delay:300ms]" />
    </span>
  );
}

// ── DnD types and helpers ────────────────────────────────

type DragZone = 'pinned' | 'regular' | 'archive';

interface SessionDragData {
  type: 'session';
  session: SessionSummary;
  zone: DragZone;
  folderId?: string | null;
}

function DroppableZone({
  id,
  children,
  className,
  activeHint,
}: {
  id: string;
  children: React.ReactNode;
  className?: string;
  activeHint?: string;
}) {
  const { setNodeRef, isOver } = useDroppable({ id });

  return (
    <div
      ref={setNodeRef}
      className={cn(
        className,
        'transition-all duration-200 rounded-lg',
        isOver && 'bg-primary/5 ring-1 ring-primary/15',
      )}
    >
      {children}
      {isOver && activeHint && (
        <div className="px-3 py-1.5 text-[11px] text-primary/40 text-center font-medium">{activeHint}</div>
      )}
    </div>
  );
}

async function performDragTransfer(sourceData: SessionDragData, targetId: string) {
  const sessionId = sourceData.session.id;
  const sourceZone = sourceData.zone;
  const store = useStore.getState();

  // Dropped on a folder → move into folder (pin if needed)
  if (targetId.startsWith('folder-')) {
    const folderId = targetId.slice(7);
    if (sourceZone === 'archive') await store.unarchiveSession(sessionId);
    if (!sourceData.session.pinned) await store.pinSession(sessionId);
    await store.moveSessionToFolder(sessionId, folderId);
    return;
  }

  // Dropped on pinned zone
  if (targetId === 'pinned-zone' && sourceZone !== 'pinned') {
    if (sourceZone === 'archive') await store.unarchiveSession(sessionId);
    await store.pinSession(sessionId);
    return;
  }

  // Dropped on pinned zone from folder → remove from folder (stay pinned)
  if (targetId === 'pinned-zone' && sourceData.folderId) {
    await store.moveSessionToFolder(sessionId, null);
    return;
  }

  // Dropped on regular zone
  if (targetId === 'regular-zone' && sourceZone !== 'regular') {
    if (sourceData.folderId) await store.moveSessionToFolder(sessionId, null);
    if (sourceZone === 'pinned') await store.unpinSession(sessionId);
    if (sourceZone === 'archive') await store.unarchiveSession(sessionId);
    return;
  }

  // Dropped on archive zone
  if (targetId === 'archive-zone' && sourceZone !== 'archive') {
    if (sourceData.folderId) await store.moveSessionToFolder(sessionId, null);
    if (sourceZone === 'pinned') await store.unpinSession(sessionId);
    await store.archiveSession(sessionId);
  }
}

// ── Session context menu (Delete + Move to folder) ───────

function SessionContextMenu({
  session,
  children,
}: {
  session: SessionSummary;
  children: React.ReactNode;
}) {
  const { deleteSession, moveSessionToFolder, profileSessions } = useStore();

  const folders = useMemo(() => {
    if (!profileSessions) return [];
    return profileSessions.pinned
      .filter((item): item is Extract<PinnedItem, { type: 'folder' }> => item.type === 'folder');
  }, [profileSessions]);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>{children}</DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-48">
        {folders.length > 0 && (
          <>
            <DropdownMenuSub>
              <DropdownMenuSubTrigger>
                <FolderOpen className="mr-2 h-4 w-4" />
                Move to folder
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent className="w-40">
                {session.folderId && (
                  <DropdownMenuItem onClick={() => void moveSessionToFolder(session.id, null)}>
                    Remove from folder
                  </DropdownMenuItem>
                )}
                {folders.map((f) => (
                  <DropdownMenuItem
                    key={f.id}
                    onClick={() => void moveSessionToFolder(session.id, f.id)}
                  >
                    {f.name}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
            <DropdownMenuSeparator />
          </>
        )}

        <DropdownMenuItem
          className="text-destructive focus:text-destructive"
          onClick={() => void deleteSession(session.id)}
        >
          <Trash2 className="mr-2 h-4 w-4" />
          Delete
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

// ── Draggable session row ────────────────────────────────

function DraggableSessionRow({
  session,
  zone,
  folderId,
  isActive,
  onSelect,
}: {
  session: SessionSummary;
  zone: DragZone;
  folderId?: string | null;
  isActive: boolean;
  onSelect: () => void;
}) {
  const { attributes, listeners, setNodeRef, isDragging } = useDraggable({
    id: `session-${session.id}`,
    data: { type: 'session', session, zone, folderId } satisfies SessionDragData,
  });

  const runtimeSessions = useRuntimeStore((state) => state.sessions);
  const isStreaming = runtimeSessions[session.id]?.state === 'streaming';

  return (
    <div
      ref={setNodeRef}
      {...listeners}
      {...attributes}
      className={cn(
        'group grid w-full min-w-0 grid-cols-[minmax(0,1fr)_auto] items-center gap-2 overflow-hidden rounded-lg px-3 py-[7px] text-[13px] cursor-grab active:cursor-grabbing transition-all duration-150',
        isActive
          ? 'bg-background shadow-sm text-foreground font-medium ring-1 ring-border/50'
          : 'text-foreground/70 hover:text-foreground hover:bg-accent/60',
        isDragging && 'opacity-20',
      )}
      onClick={onSelect}
      title={session.title || 'New Chat'}
    >
      <div className="flex min-w-0 flex-1 items-center gap-2 overflow-hidden">
        {isStreaming && <StreamingDots />}
        <span className="block min-w-0 flex-1 truncate">
          {session.title || 'New Chat'}
        </span>
      </div>

      <SessionContextMenu session={session}>
        <Button
          variant="ghost"
          size="icon"
          className="h-6 w-6 flex-shrink-0 opacity-0 group-hover:opacity-100 transition-opacity"
          onClick={(e) => e.stopPropagation()}
          onPointerDown={(e) => e.stopPropagation()}
        >
          <MoreHorizontal className="h-3.5 w-3.5 text-muted-foreground" />
        </Button>
      </SessionContextMenu>
    </div>
  );
}

// ── Folder row (collapsible, droppable) ──────────────────

function FolderRow({
  folder,
  currentSessionId,
  onSelectSession,
}: {
  folder: Extract<PinnedItem, { type: 'folder' }>;
  currentSessionId: string | null;
  onSelectSession: (id: string) => void;
}) {
  const [isOpen, setIsOpen] = useState(false);
  const { deleteFolder, updateFolder } = useStore();
  const [isEditing, setIsEditing] = useState(false);
  const [editName, setEditName] = useState(folder.name);
  const { setNodeRef, isOver } = useDroppable({ id: `folder-${folder.id}` });

  const handleRename = () => {
    if (editName.trim() && editName.trim() !== folder.name) {
      void updateFolder(folder.id, { name: editName.trim() });
    }
    setIsEditing(false);
  };

  return (
    <div ref={setNodeRef}>
      <div
        className={cn(
          'group flex w-full min-w-0 items-center justify-between gap-2 rounded-lg px-3 py-[7px] text-[13px] font-medium hover:bg-accent/60 cursor-pointer transition-all duration-150',
          isOver && 'bg-primary/10 ring-1 ring-primary/30',
        )}
        onClick={() => setIsOpen(!isOpen)}
      >
        <div className="flex min-w-0 flex-1 items-center gap-2">
          {isOpen ? (
            <ChevronDown className="h-3 w-3 flex-shrink-0 text-muted-foreground/50" />
          ) : (
            <ChevronRight className="h-3 w-3 flex-shrink-0 text-muted-foreground/50" />
          )}
          {isOpen ? (
            <FolderOpen className="h-4 w-4 flex-shrink-0 text-sky-500/80" />
          ) : (
            <FolderClosed className="h-4 w-4 flex-shrink-0 text-sky-500/80" />
          )}
          {isEditing ? (
            <Input
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
              onBlur={handleRename}
              onKeyDown={(e) => {
                if (e.key === 'Enter') handleRename();
                if (e.key === 'Escape') setIsEditing(false);
              }}
              className="h-5 text-sm py-0 px-1 w-full"
              autoFocus
              onClick={(e) => e.stopPropagation()}
            />
          ) : (
            <span className="block min-w-0 flex-1 truncate">{folder.name}</span>
          )}
          <span className="text-[10px] text-muted-foreground/40">{folder.sessions.length}</span>
        </div>

        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              size="icon"
              className="h-6 w-6 flex-shrink-0 opacity-0 group-hover:opacity-100 transition-opacity"
              onClick={(e) => e.stopPropagation()}
            >
              <MoreHorizontal className="h-3.5 w-3.5 text-muted-foreground" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-36">
            <DropdownMenuItem onClick={() => { setIsEditing(true); setEditName(folder.name); }}>
              Rename
            </DropdownMenuItem>
            <DropdownMenuItem
              className="text-destructive focus:text-destructive"
              onClick={() => void deleteFolder(folder.id)}
            >
              Delete
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>

      {isOpen && (
        <div className="ml-5 space-y-0.5">
          {folder.sessions.map((s) => (
            <DraggableSessionRow
              key={s.id}
              session={s}
              zone="pinned"
              folderId={folder.id}
              isActive={currentSessionId === s.id}
              onSelect={() => onSelectSession(s.id)}
            />
          ))}
          {folder.sessions.length === 0 && (
            <div className="text-xs text-muted-foreground/50 px-3 py-1">Empty</div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Arc-style profile space indicators ───────────────────

function ProfileSpaces({
  profiles,
  currentProfileId,
  onSwitch,
  onCreateProfile,
}: {
  profiles: ProfileInfo[];
  currentProfileId: string | null;
  onSwitch: (id: string) => void;
  onCreateProfile: () => void;
}) {
  if (profiles.length <= 1) return null;

  const currentProfile = profiles.find((p) => p.id === currentProfileId);

  return (
    <div className="border-b border-border/40">
      <div className="flex items-center gap-2 px-3.5 pt-3 pb-1">
        {profiles.map((p, i) => {
          const Icon = getProfileIcon(p);
          const isActive = p.id === currentProfileId;
          return (
            <button
              key={p.id}
              onClick={() => onSwitch(p.id)}
              className={cn(
                'relative flex items-center justify-center h-8 w-8 rounded-full text-white transition-all duration-200',
                profileColor(i),
                isActive
                  ? cn('ring-2 ring-offset-2 ring-offset-background shadow-md scale-110', PROFILE_RING_COLORS[i % PROFILE_RING_COLORS.length])
                  : 'opacity-50 hover:opacity-90 hover:scale-105',
              )}
              title={p.name}
            >
              <Icon className="h-4 w-4" strokeWidth={2.5} />
            </button>
          );
        })}
        <button
          onClick={onCreateProfile}
          className="flex items-center justify-center h-8 w-8 rounded-full border-2 border-dashed border-muted-foreground/20 text-muted-foreground/30 hover:text-muted-foreground/60 hover:border-muted-foreground/40 transition-all duration-200"
          title="New profile"
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
      </div>
      {currentProfile && (
        <div className="px-4 pb-2.5 pt-1 min-w-0">
          <span className="block truncate text-[11px] font-semibold text-muted-foreground/60 tracking-wide">
            {currentProfile.name}
          </span>
        </div>
      )}
    </div>
  );
}

// ── Main Sidebar component ───────────────────────────────

export function Sidebar({ className, onOpenSettings, onSelectSession, onNewChat }: SidebarProps) {
  const {
    currentSessionId,
    setCurrentSession,
    loaded,
    profileSessionsLoading,
    profileAccessBlockedReason,
    backendUnavailableReason,
    containerPrepareStatuses,
    profiles,
    currentProfileId,
    profileSessions,
    switchProfile,
    createFolder,
  } = useStore();

  const [searchQuery] = useState('');
  const [archivedOpen, setArchivedOpen] = useState(false);
  const [showNewProfile, setShowNewProfile] = useState(false);
  const [newProfileName, setNewProfileName] = useState('');
  const [activeDragSession, setActiveDragSession] = useState<SessionSummary | null>(null);

  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 8 },
    }),
  );

  const handleSelectSession = useCallback(
    (id: string) => {
      setCurrentSession(id);
      onSelectSession?.(id);
    },
    [setCurrentSession, onSelectSession],
  );

  const handleCreateProfile = useCallback(async () => {
    if (!newProfileName.trim()) return;
    const { createProfile: create, switchProfile: sw } = useStore.getState();
    const profile = await create({ name: newProfileName.trim() });
    setNewProfileName('');
    setShowNewProfile(false);
    await sw(profile.id);
  }, [newProfileName]);

  const filterSessions = useCallback(
    (sessions: SessionSummary[]): SessionSummary[] => {
      if (!searchQuery.trim()) return sessions;
      const lower = searchQuery.toLowerCase();
      return sessions.filter(
        (s) =>
          (s.title ?? '').toLowerCase().includes(lower) ||
          s.contentPreview.toLowerCase().includes(lower),
      );
    },
    [searchQuery],
  );

  const pinnedItems = useMemo(
    () => profileSessions?.pinned ?? [],
    [profileSessions],
  );

  const regularSessions = useMemo(
    () => filterSessions(profileSessions?.regular ?? []),
    [profileSessions, filterSessions],
  );

  const archivedSessions = useMemo(
    () => filterSessions(profileSessions?.archived ?? []),
    [profileSessions, filterSessions],
  );

  const regularGroups = useMemo(
    () => groupByTime(regularSessions),
    [regularSessions],
  );

  function handleDragStart(event: DragStartEvent) {
    const data = event.active.data.current as SessionDragData | undefined;
    if (data?.type === 'session') {
      setActiveDragSession(data.session);
    }
  }

  function handleDragEnd(event: DragEndEvent) {
    setActiveDragSession(null);

    const { active, over } = event;
    if (!over) return;

    const sourceData = active.data.current as SessionDragData | undefined;
    if (!sourceData || sourceData.type !== 'session') return;

    const targetId = String(over.id);

    // No-op if dropped on own zone without meaningful change
    if (
      (targetId === 'pinned-zone' && sourceData.zone === 'pinned' && !sourceData.folderId) ||
      (targetId === 'regular-zone' && sourceData.zone === 'regular') ||
      (targetId === 'archive-zone' && sourceData.zone === 'archive')
    ) {
      return;
    }

    void performDragTransfer(sourceData, targetId);
  }

  const isDragging = activeDragSession !== null;
  const currentProfile = profiles.find((profile) => profile.id === currentProfileId) ?? null;
  const containerPrepareStatus =
    currentProfileId ? containerPrepareStatuses[currentProfileId] : undefined;
  const workspaceSurface = resolveWorkspaceSurfaceState({
    loaded,
    profileSessionsLoading,
    profile: currentProfile,
    status: containerPrepareStatus,
    blockedReason: profileAccessBlockedReason,
    backendUnavailableReason,
  });
  const containerPrepareMessage = workspaceSurface.container.summary;
  const showBackendUnavailable = workspaceSurface.state === 'backend-down';
  const showContainerPreparation = workspaceSurface.state === 'container-preparing';
  const showStandardLoading = workspaceSurface.state === 'profile-loading';
  const canBrowseSessions = workspaceSurface.state === 'ready';
  const canCreateSession = canBrowseSessions;

  return (
    <div
      className={cn(
        'h-full w-64 flex-shrink-0 flex flex-col border-r border-border/45 bg-background/85 backdrop-blur-xl supports-[backdrop-filter]:bg-background/70 shadow-sm',
        className,
      )}
    >
      {/* Profile spaces (Arc-style colored icons) */}
      <ProfileSpaces
        profiles={profiles}
        currentProfileId={currentProfileId}
        onSwitch={(id) => {
          if (id === currentProfileId) return;
          setCurrentSession(null);
          onNewChat?.();
          void switchProfile(id);
        }}
        onCreateProfile={() => setShowNewProfile(true)}
      />

      {/* New profile inline form */}
      {showNewProfile && (
        <div className="px-3 py-2 border-b flex gap-2">
          <Input
            value={newProfileName}
            onChange={(e) => setNewProfileName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void handleCreateProfile();
              if (e.key === 'Escape') setShowNewProfile(false);
            }}
            placeholder="Profile name..."
            className="h-7 text-xs"
            autoFocus
          />
          <Button
            size="sm"
            className="h-7 text-xs px-2"
            onClick={() => void handleCreateProfile()}
          >
            Add
          </Button>
        </div>
      )}

      <DndContext
        sensors={sensors}
        onDragStart={handleDragStart}
        onDragEnd={handleDragEnd}
      >
        {/* Scrollable content */}
        <ScrollArea className="min-h-0 flex-1">
          <div className="py-2">
            {showBackendUnavailable ? (
              <div className="px-3 py-6">
                <div className="rounded-xl border border-red-500/50 bg-background/75 p-4 text-center">
                  <div className="text-sm font-medium">Backend unavailable</div>
                  <div className="mt-1 text-xs text-muted-foreground">
                    Chat history and new chat are temporarily disabled.
                  </div>
                  <div className="mt-3 rounded-md bg-muted/40 px-2 py-1.5 text-left text-[11px] text-muted-foreground">
                    {backendUnavailableReason}
                  </div>
                </div>
              </div>
            ) : showStandardLoading ? (
              <div className="space-y-2 px-3 py-2">
                {Array.from({ length: 6 }).map((_, i) => (
                  <div key={i} className="h-8 rounded bg-muted/50 animate-pulse" />
                ))}
              </div>
            ) : showContainerPreparation ? (
              <div className="px-3 py-6">
                <div className="rounded-xl border border-border/60 bg-background/70 p-4 text-center">
                  <div className="text-sm font-medium">Preparing workspace container</div>
                  <div className="mt-1 text-xs text-muted-foreground">
                    Chat history and new chat are temporarily disabled for this profile.
                  </div>
                  <div className="mt-3 rounded-md bg-muted/40 px-2 py-1.5 text-left text-[11px] text-muted-foreground">
                    {containerPrepareMessage}
                  </div>
                </div>
              </div>
            ) : canBrowseSessions ? (
              <div>
                {/* ── Pinned zone (droppable) ── */}
                <DroppableZone
                  id="pinned-zone"
                  className="px-1 min-h-[8px]"
                  activeHint={pinnedItems.length === 0 && isDragging ? 'Drop to pin' : undefined}
                >
                  {pinnedItems.length > 0 && (
                    <div className="space-y-0.5">
                      {pinnedItems.map((item) => {
                        if (item.type === 'session') {
                          return (
                            <DraggableSessionRow
                              key={item.id}
                              session={item}
                              zone="pinned"
                              isActive={currentSessionId === item.id}
                              onSelect={() => handleSelectSession(item.id)}
                            />
                          );
                        }
                        return (
                          <FolderRow
                            key={item.id}
                            folder={item}
                            currentSessionId={currentSessionId}
                            onSelectSession={handleSelectSession}
                          />
                        );
                      })}
                    </div>
                  )}
                </DroppableZone>

                {/* ── Separator ── */}
                <Separator className="mx-3.5 my-1.5 opacity-60" />

                {/* ── New Chat button ── */}
                <button
                  onClick={() => {
                    setCurrentSession(null);
                    onNewChat?.();
                  }}
                  className="mx-2 flex w-[calc(100%-1rem)] min-w-0 items-center gap-2 rounded-lg px-2.5 py-[7px] text-[13px] text-muted-foreground/60 transition-all duration-150 hover:bg-accent/50 hover:text-foreground"
                >
                  <Plus className="h-3.5 w-3.5" />
                  <span className="truncate">New Chat</span>
                </button>

                {/* ── Regular sessions zone (droppable) ── */}
                <DroppableZone
                  id="regular-zone"
                  className="mt-1 min-h-[40px]"
                  activeHint={regularSessions.length === 0 && isDragging ? 'Drop here' : undefined}
                >
                  {TIME_GROUP_ORDER.map((group) => {
                    const items = regularGroups.get(group);
                    if (!items || items.length === 0) return null;
                    return (
                      <div key={group}>
                        <div className="px-4 pt-3.5 pb-1 text-[11px] font-semibold text-muted-foreground/40 uppercase tracking-wider">
                          {group}
                        </div>
                        <div className="px-1 space-y-0.5">
                          {items.map((s) => (
                            <DraggableSessionRow
                              key={s.id}
                              session={s}
                              zone="regular"
                              isActive={currentSessionId === s.id}
                              onSelect={() => handleSelectSession(s.id)}
                            />
                          ))}
                        </div>
                      </div>
                    );
                  })}

                  {regularSessions.length === 0 && !isDragging && (
                    <div className="text-center text-xs text-muted-foreground/50 py-4">
                      No recent chats
                    </div>
                  )}
                </DroppableZone>

                {/* ── Archived section (collapsible, droppable) ── */}
                <DroppableZone
                  id="archive-zone"
                  className="mt-2"
                  activeHint={!archivedOpen && isDragging ? 'Drop to archive' : undefined}
                >
                  {(archivedSessions.length > 0 || isDragging) && (
                    <>
                      <button
                        onClick={() => setArchivedOpen(!archivedOpen)}
                        className="flex w-full min-w-0 items-center gap-1.5 px-4 py-1.5 text-[11px] font-semibold text-muted-foreground/30 uppercase tracking-wider hover:text-muted-foreground/60 transition-all duration-150"
                      >
                        {archivedOpen ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
                        <span className="truncate">Archived</span>
                        <span className="text-muted-foreground/20">{archivedSessions.length}</span>
                      </button>
                      {archivedOpen && (
                        <div className="px-1 space-y-0.5">
                          {archivedSessions.map((s) => (
                            <DraggableSessionRow
                              key={s.id}
                              session={s}
                              zone="archive"
                              isActive={currentSessionId === s.id}
                              onSelect={() => handleSelectSession(s.id)}
                            />
                          ))}
                        </div>
                      )}
                    </>
                  )}
                </DroppableZone>
              </div>
            ) : null}
          </div>
        </ScrollArea>

        {/* Drag overlay (floating card) */}
        <DragOverlay>
          {activeDragSession && (
            <div className="bg-background/95 backdrop-blur-md border border-border/50 rounded-lg px-3 py-2 text-[13px] shadow-2xl max-w-56 truncate font-medium">
              {activeDragSession.title || 'New Chat'}
            </div>
          )}
        </DragOverlay>
      </DndContext>

      {/* Bottom bar (settings left, + right with dropdown) */}
      <div className="mt-auto px-3 py-2 border-t border-border/40 bg-background/80 backdrop-blur-md flex items-center justify-between">
        <button
          onClick={onOpenSettings}
          className="flex items-center justify-center h-8 w-8 rounded-lg text-muted-foreground/40 hover:text-foreground hover:bg-accent/60 transition-all duration-150"
          title="Settings"
        >
          <Settings className="h-4 w-4" />
        </button>

        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              disabled={!canCreateSession}
              className="flex items-center justify-center h-8 w-8 rounded-lg text-muted-foreground/40 hover:text-foreground hover:bg-accent/60 transition-all duration-150"
              title="New"
            >
              <Plus className="h-4 w-4" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-40">
            {canCreateSession ? (
              <>
                <DropdownMenuItem onClick={() => { setCurrentSession(null); onNewChat?.(); }}>
                  <Plus className="mr-2 h-4 w-4" />
                  New Chat
                </DropdownMenuItem>
                <DropdownMenuItem onClick={() => void createFolder('New Folder')}>
                  <FolderPlus className="mr-2 h-4 w-4" />
                  New Folder
                </DropdownMenuItem>
              </>
            ) : (
              <DropdownMenuItem disabled>
                {showBackendUnavailable ? 'Backend unavailable' : 'Container not ready'}
              </DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </div>
  );
}
