import { useNavigate } from 'react-router-dom';
import { Button } from '@/components/ui/button';
import { useSessionsStore } from '@/lib/stores/sessions';

export function HomePage() {
  const createNew = useSessionsStore((s) => s.createNew);
  const navigate = useNavigate();

  const start = async () => {
    const s = await createNew();
    navigate(`/s/${s.id}`);
  };

  return (
    <div className="flex h-full items-center justify-center px-6">
      <div className="max-w-md space-y-4 text-center">
        <div className="text-2xl font-semibold tracking-tight">Welcome to May</div>
        <p className="text-sm text-muted-foreground">
          Terminal-first agent. Pick a session from the sidebar or start a new one.
        </p>
        <Button onClick={start}>New chat</Button>
      </div>
    </div>
  );
}
