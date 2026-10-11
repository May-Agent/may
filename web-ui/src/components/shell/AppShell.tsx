import { useEffect, useRef } from 'react';
import { Outlet } from 'react-router-dom';
import { Toaster } from 'sonner';
import { Sidebar } from './Sidebar';
import { CommandPalette } from './CommandPalette';
import { BrowserLiveView } from '@/components/browser/BrowserLiveView';
import { TooltipProvider } from '@/components/ui/tooltip';
import { InteractionLayer } from '@/components/interactions/InteractionLayer';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useProfilesStore, writeLastProfileId } from '@/lib/stores/profiles';
import { useConfigStore } from '@/lib/stores/config';
import { useModelsStore } from '@/lib/stores/models';
import { applyTheme, useUiStore } from '@/lib/stores/ui';
import { getBootstrap } from '@/lib/api/bootstrap';
import { useActiveProfile } from '@/lib/profile/active';

export function AppShell() {
  const connect = useRuntimeStore((s) => s.connect);
  const disconnect = useRuntimeStore((s) => s.disconnect);
  const reconnectForProfile = useRuntimeStore((s) => s.reconnectForProfile);
  const seedProfileSessions = useRuntimeStore((s) => s.seedProfileSessions);
  const loadFromBootstrap = useProfilesStore((s) => s.loadFromBootstrap);
  const setConfig = useConfigStore((s) => s.setConfig);
  const theme = useUiStore((s) => s.theme);
  const toasterTheme = theme === 'system' ? undefined : theme;
  const { profileId } = useActiveProfile();

  const bootstrappedRef = useRef(false);

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  useEffect(() => {
    if (bootstrappedRef.current) return;
    bootstrappedRef.current = true;
    let cancelled = false;
    // Kick off a background models refresh on every app boot. The store
    // already exposes a (possibly stale) cached value via persist, so the
    // ModelPicker renders instantly; this updates it once fresh data lands.
    void useModelsStore.getState().refresh();
    (async () => {
      try {
        const resp = await getBootstrap();
        if (cancelled) return;
        loadFromBootstrap(resp);
        setConfig(resp.config);
        if (resp.defaultProfileSessions) {
          seedProfileSessions(resp.defaultProfileSessions.profile.id, resp.defaultProfileSessions);
        }
      } catch {
        // bootstrap failed; SSE will attempt to connect when profileId resolves
      }
    })();
    return () => {
      cancelled = true;
      disconnect();
    };
  }, [disconnect, loadFromBootstrap, seedProfileSessions, setConfig]);

  useEffect(() => {
    if (!profileId) return;
    writeLastProfileId(profileId);
    const { activeProfileId: currentStream, connected } = useRuntimeStore.getState();
    if (currentStream === profileId && connected) return;
    if (!connected) {
      void connect(profileId);
    } else {
      void reconnectForProfile(profileId);
    }
  }, [profileId, connect, reconnectForProfile]);

  return (
    <TooltipProvider delayDuration={200}>
      <div className="flex h-full w-full">
        <Sidebar />
        <main className="flex-1 overflow-hidden">
          <Outlet />
        </main>
        <BrowserLiveView />
        <InteractionLayer />
        <CommandPalette />
        <Toaster theme={toasterTheme} richColors closeButton position="top-right" />
      </div>
    </TooltipProvider>
  );
}
