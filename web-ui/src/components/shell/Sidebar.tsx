import { useEffect } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { MessageSquare, Plus, Settings as SettingsIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { useSessionsStore } from '@/lib/stores/sessions';
import { cn } from '@/lib/utils';
import { formatRelativeTime } from '@/lib/utils';

export function Sidebar() {
  const navigate = useNavigate();
  const { sessionId } = useParams();
  const order = useSessionsStore((s) => s.order);
  const index = useSessionsStore((s) => s.index);
  const load = useSessionsStore((s) => s.load);
  const createNew = useSessionsStore((s) => s.createNew);

  useEffect(() => {
    load();
  }, [load]);

  const handleCreate = async () => {
    const s = await createNew();
    navigate(`/s/${s.id}`);
  };

  return (
    <aside className="flex h-full w-64 flex-col border-r border-border bg-card/40">
      <div className="flex items-center justify-between px-4 py-3">
        <Link to="/" className="text-sm font-semibold tracking-tight">
          May
        </Link>
        <Button size="icon" variant="ghost" onClick={handleCreate} aria-label="New chat">
          <Plus className="size-4" />
        </Button>
      </div>
      <ScrollArea className="flex-1 px-2">
        <ul className="space-y-0.5 pb-4">
          {order.map((id) => {
            const entry = index[id];
            if (!entry) return null;
            const active = id === sessionId;
            return (
              <li key={id}>
                <Link
                  to={`/s/${id}`}
                  className={cn(
                    'flex items-start gap-2 rounded-md px-2.5 py-2 text-sm transition-colors hover:bg-accent',
                    active && 'bg-accent',
                  )}
                >
                  <MessageSquare className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                  <div className="min-w-0 flex-1">
                    <div className="truncate font-medium leading-tight">{entry.title}</div>
                    <div className="text-[11px] text-muted-foreground">
                      {formatRelativeTime(entry.lastModified)}
                    </div>
                  </div>
                </Link>
              </li>
            );
          })}
          {order.length === 0 && (
            <li className="px-2 py-8 text-center text-xs text-muted-foreground">
              No sessions yet
            </li>
          )}
        </ul>
      </ScrollArea>
      <div className="border-t border-border p-2">
        <Link
          to="/settings"
          className="flex items-center gap-2 rounded-md px-2.5 py-2 text-sm hover:bg-accent"
        >
          <SettingsIcon className="size-4 text-muted-foreground" />
          Settings
        </Link>
      </div>
    </aside>
  );
}
