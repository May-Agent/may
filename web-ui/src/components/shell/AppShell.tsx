import { useEffect } from 'react';
import { Outlet } from 'react-router-dom';
import { Sidebar } from './Sidebar';
import { TooltipProvider } from '@/components/ui/tooltip';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { applyTheme, useUiStore } from '@/lib/stores/ui';

export function AppShell() {
  const connect = useRuntimeStore((s) => s.connect);
  const disconnect = useRuntimeStore((s) => s.disconnect);
  const theme = useUiStore((s) => s.theme);

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  useEffect(() => {
    connect().catch(() => undefined);
    return () => disconnect();
  }, [connect, disconnect]);

  return (
    <TooltipProvider delayDuration={200}>
      <div className="flex h-full w-full">
        <Sidebar />
        <main className="flex-1 overflow-hidden">
          <Outlet />
        </main>
      </div>
    </TooltipProvider>
  );
}
