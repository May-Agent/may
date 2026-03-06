import { AlertTriangle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useStore } from '@/lib/store';

export function DangerSection() {
  const { deleteAllSessions } = useStore();

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-destructive tracking-wide">
        <AlertTriangle className="h-4 w-4" /> Danger Zone
      </div>
      <div className="rounded border border-destructive/40 bg-destructive/5 p-4 flex flex-col gap-3 md:flex-row md:items-center md:justify-between">
        <div>
          <div className="font-medium">Delete all sessions</div>
          <div className="text-xs text-muted-foreground">This removes every saved chat session and creates a fresh empty one.</div>
        </div>
        <Button
          variant="destructive"
          onClick={async () => {
            if (!window.confirm('Delete all sessions? This cannot be undone.')) return;
            await deleteAllSessions();
          }}
        >
          Delete all sessions
        </Button>
      </div>
    </section>
  );
}
