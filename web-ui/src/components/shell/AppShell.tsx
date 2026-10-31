import { useEffect, useRef } from 'react';
import { Outlet, useLocation } from 'react-router-dom';
import { Toaster } from 'sonner';
import { Sidebar } from './Sidebar';
import { Sheet, SheetContent, SheetTitle } from '@/components/ui/sheet';
import { isMacPlatform, matchShortcut } from '@/lib/shortcuts';
import { useNewChat } from '@/lib/useNewChat';
import { INSPECTOR_COLUMN_QUERY, SIDEBAR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';
import { useTranslation } from '@/lib/i18n';
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
  const sidebarCollapsed = useUiStore((s) => s.sidebarCollapsed);
  const sidebarSheetOpen = useUiStore((s) => s.sidebarSheetOpen);
  const setSidebarSheetOpen = useUiStore((s) => s.setSidebarSheetOpen);
  const sidebarColumn = useMediaQuery(SIDEBAR_COLUMN_QUERY);
  const { t } = useTranslation();
  const location = useLocation();

  const bootstrappedRef = useRef(false);

  useEffect(() => {
    applyTheme(theme);
    if (theme !== 'system') return;
    // Follow the system appearance live while the theme is "system".
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTheme('system');
    media.addEventListener('change', onChange);
    return () => media.removeEventListener('change', onChange);
  }, [theme]);

  // Navigating from the sidebar sheet dismisses it.
  useEffect(() => {
    setSidebarSheetOpen(false);
  }, [location.pathname, setSidebarSheetOpen]);

  useGlobalShortcuts(sidebarColumn);

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
      <div className="flex h-full w-full overflow-hidden">
        {sidebarColumn && !sidebarCollapsed && <Sidebar />}
        {!sidebarColumn && (
          <Sheet open={sidebarSheetOpen} onOpenChange={setSidebarSheetOpen}>
            <SheetContent side="left" className="w-[min(20rem,85vw)] max-w-none p-0" hideCloseButton aria-describedby={undefined}>
              <SheetTitle className="sr-only">{t('sidebar.sessions')}</SheetTitle>
              <Sidebar inSheet />
            </SheetContent>
          </Sheet>
        )}
        <main className="min-w-0 flex-1 overflow-hidden">
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

/** Cmd/Ctrl+N, Cmd/Ctrl+K and the sidebar / inspector toggles. */
function useGlobalShortcuts(sidebarColumn: boolean) {
  const { start } = useNewChat();

  useEffect(() => {
    const mac = isMacPlatform();
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing) return;
      const id = matchShortcut(e, mac);
      if (!id) return;
      const ui = useUiStore.getState();
      e.preventDefault();
      switch (id) {
        case 'newChat':
          void start();
          break;
        case 'search':
          ui.setCommandOpen(!ui.commandOpen);
          break;
        case 'toggleSidebar':
          if (sidebarColumn) ui.toggleSidebar();
          else ui.setSidebarSheetOpen(!ui.sidebarSheetOpen);
          break;
        case 'toggleInspector':
          if (window.matchMedia(INSPECTOR_COLUMN_QUERY).matches) ui.toggleInspector();
          else ui.setInspectorSheetOpen(!ui.inspectorSheetOpen);
          break;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [sidebarColumn, start]);
}
