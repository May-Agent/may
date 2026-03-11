import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { Navigate, Route, Routes, useLocation, useNavigate, useParams } from 'react-router-dom';
import { Sidebar } from '@/components/layout/Sidebar';
import { SettingsPage } from '@/components/layout/SettingsPage';
import { ChatInterface } from '@/components/chat/ChatInterface';
import { sendChat, subscribeUiEvents } from '@/lib/api';
import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { ProfileSessions } from '@/types';

function profileHasSession(profileSessions: ProfileSessions | null, sessionId: string): boolean {
  if (!profileSessions) return false;
  if (profileSessions.regular.some((session) => session.id === sessionId)) return true;
  if (profileSessions.archived.some((session) => session.id === sessionId)) return true;
  for (const item of profileSessions.pinned) {
    if (item.type === 'session' && item.id === sessionId) {
      return true;
    }
    if (item.type === 'folder' && item.sessions.some((session) => session.id === sessionId)) {
      return true;
    }
  }
  return false;
}

function AppScaffold({
  isSidebarOpen,
  setSidebarOpen,
  isDesktopSidebarOpen,
  children,
  onOpenSettings,
}: {
  isSidebarOpen: boolean;
  setSidebarOpen: (value: boolean) => void;
  isDesktopSidebarOpen: boolean;
  children: ReactNode;
  onOpenSettings: () => void;
}) {
  const navigate = useNavigate();

  return (
    <div className="flex h-full min-h-0 bg-background text-foreground overflow-hidden">
      <div className="md:hidden fixed top-4 left-4 z-50">
        <Sheet open={isSidebarOpen} onOpenChange={setSidebarOpen}>
          <SheetTrigger asChild>
            <Button variant="outline" size="icon">
              <Menu className="h-4 w-4" />
            </Button>
          </SheetTrigger>
          <SheetContent side="left" className="p-0 w-72">
            <Sidebar
              onOpenSettings={onOpenSettings}
              onSelectSession={(id) => navigate(`/chat/${encodeURIComponent(id)}`)}
              onNewChat={() => navigate('/chat/new')}
            />
          </SheetContent>
        </Sheet>
      </div>

      <div
        className={`${isDesktopSidebarOpen ? 'w-64' : 'w-0'} hidden h-full flex-shrink-0 overflow-hidden transition-all duration-300 ease-in-out md:block`}
      >
        <Sidebar
          className="w-64"
          onOpenSettings={onOpenSettings}
          onSelectSession={(id) => navigate(`/chat/${encodeURIComponent(id)}`)}
          onNewChat={() => navigate('/chat/new')}
        />
      </div>

      <main className="relative flex h-full min-h-0 min-w-0 flex-1 flex-col overflow-hidden">{children}</main>
    </div>
  );
}

function ChatRouteView({
  isSidebarOpen,
  setSidebarOpen,
  isDesktopSidebarOpen,
  setDesktopSidebarOpen,
  onOpenSettings,
}: {
  isSidebarOpen: boolean;
  setSidebarOpen: (value: boolean) => void;
  isDesktopSidebarOpen: boolean;
  setDesktopSidebarOpen: (value: boolean) => void;
  onOpenSettings: () => void;
}) {
  const { sessionId } = useParams();
  const navigate = useNavigate();
  const {
    loaded,
    currentSessionId,
    currentProfileId,
    profileSessions,
    profileSessionsLoading,
    setCurrentSession,
  } = useStore();

  useEffect(() => {
    if (!sessionId || sessionId === 'new') {
      if (currentSessionId !== null) {
        setCurrentSession(null);
      }
      return;
    }

    if (!loaded || profileSessionsLoading) return;

    const existsInProfile = profileHasSession(profileSessions, sessionId);
    if (!existsInProfile) {
      if (currentSessionId !== null) {
        setCurrentSession(null);
      }
      navigate('/chat/new', { replace: true });
      return;
    }

    if (currentSessionId !== sessionId) {
      setCurrentSession(sessionId);
    }
  }, [
    loaded,
    sessionId,
    currentSessionId,
    profileSessions,
    profileSessionsLoading,
    setCurrentSession,
    navigate,
  ]);

  return (
    <AppScaffold
      isSidebarOpen={isSidebarOpen}
      setSidebarOpen={setSidebarOpen}
      isDesktopSidebarOpen={isDesktopSidebarOpen}
      onOpenSettings={onOpenSettings}
    >
      <ChatInterface
        key={`${currentProfileId ?? 'none'}:${sessionId ?? 'new'}`}
        onToggleSidebar={() => setDesktopSidebarOpen(!isDesktopSidebarOpen)}
        isSidebarOpen={isDesktopSidebarOpen}
      />
    </AppScaffold>
  );
}

function SettingsRouteView() {
  return (
    <div className="h-full min-h-0 bg-background text-foreground overflow-auto">
      <SettingsPage />
    </div>
  );
}

function AppShell() {
  const location = useLocation();
  const navigate = useNavigate();
  const [isSidebarOpen, setSidebarOpen] = useState(false);
  const [isDesktopSidebarOpen, setDesktopSidebarOpen] = useState(true);

  const {
    settings,
    loadInitialData,
    refreshRuntimeSettings,
    refreshFullConfig,
    refreshMcpServers,
    refreshSkills,
    currentProfileId,
    markBackendUnavailable,
    clearBackendUnavailable,
  } = useStore();

  useEffect(() => {
    void loadInitialData();
  }, [loadInitialData]);

  useEffect(() => {
    if (location.pathname === '/settings') {
      void refreshRuntimeSettings();
      void refreshFullConfig();
      void refreshMcpServers();
      void refreshSkills();
    }
  }, [location.pathname, refreshRuntimeSettings, refreshFullConfig, refreshMcpServers, refreshSkills]);

  useEffect(() => {
    if (!('serviceWorker' in navigator)) {
      return;
    }

    const handleServiceWorkerMessage = (event: MessageEvent) => {
      const data = event.data as
        | {
            type?: string;
            payload?: {
              sessionId?: unknown;
              callbackContextXml?: unknown;
              callbackMessage?: unknown;
            };
          }
        | undefined;

      if (!data || data.type !== 'may-notification-click') {
        return;
      }

      void (async () => {
        const store = useStore.getState();
        try {
          const payload = data.payload ?? {};
          const requestedSessionId =
            typeof payload.sessionId === 'string' && payload.sessionId.trim().length > 0
              ? payload.sessionId
              : undefined;
          const callbackMessage =
            typeof payload.callbackMessage === 'string' ? payload.callbackMessage.trim() : '';
          const callbackContextXml =
            typeof payload.callbackContextXml === 'string'
              ? payload.callbackContextXml.trim()
              : '';

          const existingSession = requestedSessionId
            ? store.sessions.find((s) => s.id === requestedSessionId)
            : undefined;
          const targetSessionId = existingSession?.id ?? (await store.createSession());

          store.setCurrentSession(targetSessionId);
          navigate(`/chat/${encodeURIComponent(targetSessionId)}`);

          if (!callbackMessage) {
            return;
          }

          store.addMessage(targetSessionId, callbackMessage, 'user');
          const assistant = await sendChat({
            sessionId: targetSessionId,
            content: callbackMessage,
            hiddenContextXml: callbackContextXml || undefined,
            attachments: [],
          });
          store.addMessage(targetSessionId, assistant.content, 'assistant');
        } catch (error) {
          const reason = error instanceof Error ? error.message : String(error);
          store.markBackendUnavailable(reason);
          console.error('failed to process notification click callback', error);
        }
      })();
    };

    navigator.serviceWorker.addEventListener('message', handleServiceWorkerMessage);
    return () => {
      navigator.serviceWorker.removeEventListener('message', handleServiceWorkerMessage);
    };
  }, [navigate]);

  useEffect(() => {
    const unsubscribeEvents = subscribeUiEvents({
      profileId: currentProfileId ?? undefined,
      onOpen: () => {},
      onConfig: (data) => {
        clearBackendUnavailable();
        useStore.setState((state) => {
          const nextFullConfig = data.fullConfig ?? state.uiConfig.fullConfig;
          const nextFont =
            data.uiFont ??
            nextFullConfig?.appearance.font ??
            state.settings.font;
          const nextTheme =
            data.uiTheme ??
            nextFullConfig?.appearance.theme ??
            state.settings.theme;
          return {
            settings: {
              ...state.settings,
              font: nextFont,
              theme: nextTheme,
            },
            uiConfig: {
              ...state.uiConfig,
              sshServers: data.sshServers ?? state.uiConfig.sshServers,
              maxIterations:
                data.maxIterations ?? state.uiConfig.maxIterations,
              fullConfig: nextFullConfig,
            },
          };
        });
      },
      onContainerPrepare: (payload) => {
        clearBackendUnavailable();
        useStore.getState().setContainerPrepareStatuses(payload.statuses);
      },
      onSessionRuntimes: ({ sessions }) => {
        clearBackendUnavailable();
        useRuntimeStore.getState().applyRuntimes(sessions);
      },
      onHeartbeat: () => {},
      onError: () => {
        markBackendUnavailable('Backend event stream disconnected');
      },
    });

    return () => {
      unsubscribeEvents();
    };
  }, [clearBackendUnavailable, currentProfileId, markBackendUnavailable]);

  useEffect(() => {
    if (settings.font && !settings.font.startsWith('Source Han')) {
      const linkId = 'google-font-link';
      let link = document.getElementById(linkId) as HTMLLinkElement;
      if (!link) {
        link = document.createElement('link');
        link.id = linkId;
        link.rel = 'stylesheet';
        document.head.appendChild(link);
      }
      link.href = `https://fonts.googleapis.com/css2?family=${settings.font.replace(/\s+/g, '+')}:wght@400;500;600;700&display=swap`;
    }

    const root = document.documentElement;
    root.classList.remove('light', 'dark');
    root.classList.add(
      settings.theme === 'system'
        ? matchMedia('(prefers-color-scheme: dark)').matches
          ? 'dark'
          : 'light'
        : settings.theme,
    );
  }, [settings.theme, settings.font]);

  const fontFamily = useMemo(
    () => FONTS.find((f) => f.value === settings.font)?.family,
    [settings.font],
  );

  return (
    <div style={{ fontFamily }} className="h-dvh min-h-0">
      <Routes>
        <Route
          path="/chat/new"
          element={
            <ChatRouteView
              isSidebarOpen={isSidebarOpen}
              setSidebarOpen={setSidebarOpen}
              isDesktopSidebarOpen={isDesktopSidebarOpen}
              setDesktopSidebarOpen={setDesktopSidebarOpen}
              onOpenSettings={() => navigate('/settings')}
            />
          }
        />
        <Route
          path="/chat/:sessionId"
          element={
            <ChatRouteView
              isSidebarOpen={isSidebarOpen}
              setSidebarOpen={setSidebarOpen}
              isDesktopSidebarOpen={isDesktopSidebarOpen}
              setDesktopSidebarOpen={setDesktopSidebarOpen}
              onOpenSettings={() => navigate('/settings')}
            />
          }
        />
        <Route path="/settings" element={<SettingsRouteView />} />
        <Route path="/" element={<Navigate to="/chat/new" replace />} />
        <Route path="*" element={<Navigate to="/chat/new" replace />} />
      </Routes>
    </div>
  );
}

export default function App() {
  return <AppShell />;
}
