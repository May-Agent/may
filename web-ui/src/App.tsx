import {
  Suspense,
  lazy,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import {
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
  useParams,
} from 'react-router-dom';
import { Sidebar } from '@/components/layout/Sidebar';
import { ChatInterface } from '@/components/chat/ChatInterface';
import {
  consumeNotificationCallback,
  fetchSession,
  sendChat,
  subscribeUiEvents,
} from '@/lib/api';
import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { ProfileSessions } from '@/types';

const LazySettingsPage = lazy(() =>
  import('@/components/layout/SettingsPage').then((module) => ({
    default: module.SettingsPage,
  })),
);

function profileHasSession(
  profileSessions: ProfileSessions | null,
  sessionId: string,
): boolean {
  if (!profileSessions) return false;
  if (profileSessions.regular.some((session) => session.id === sessionId))
    return true;
  if (profileSessions.archived.some((session) => session.id === sessionId))
    return true;
  for (const item of profileSessions.pinned) {
    if (item.type === 'session' && item.id === sessionId) {
      return true;
    }
    if (
      item.type === 'folder' &&
      item.sessions.some((session) => session.id === sessionId)
    ) {
      return true;
    }
  }
  return false;
}

function flattenProfileSessions(profileSessions: ProfileSessions) {
  const pinnedSessions = profileSessions.pinned.flatMap((item) =>
    item.type === 'session' ? [item] : item.sessions,
  );
  return [
    ...pinnedSessions,
    ...profileSessions.regular,
    ...profileSessions.archived,
  ];
}

function AppScaffold({
  isSidebarOpen,
  setSidebarOpen,
  isDesktopSidebarOpen,
  children,
  onOpenSettings,
  hideChrome = false,
}: {
  isSidebarOpen: boolean;
  setSidebarOpen: (value: boolean) => void;
  isDesktopSidebarOpen: boolean;
  children: ReactNode;
  onOpenSettings: () => void;
  hideChrome?: boolean;
}) {
  const navigate = useNavigate();

  return (
    <div className="flex h-full min-h-0 bg-background text-foreground overflow-hidden">
      {!hideChrome && (
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
                onSelectSession={(id) =>
                  navigate(`/chat/${encodeURIComponent(id)}`)
                }
                onNewChat={() => navigate('/chat/new')}
              />
            </SheetContent>
          </Sheet>
        </div>
      )}

      {!hideChrome && (
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
      )}

      <main className="relative flex h-full min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
        {children}
      </main>
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
    backendUnavailableReason,
    profileSessions,
    profileSessionsLoading,
    setCurrentSession,
  } = useStore();
  const hideChrome = (backendUnavailableReason ?? "").trim().length > 0;

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
      hideChrome={hideChrome}
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
      <Suspense
        fallback={
          <div className="px-6 py-6 text-sm text-muted-foreground">
            Loading settings…
          </div>
        }
      >
        <LazySettingsPage />
      </Suspense>
    </div>
  );
}

function AppShell() {
  const location = useLocation();
  const navigate = useNavigate();
  const [isSidebarOpen, setSidebarOpen] = useState(false);
  const [isDesktopSidebarOpen, setDesktopSidebarOpen] = useState(true);
  const handledNotificationIdsRef = useRef<Set<string>>(new Set());

  const {
    settings,
    loadInitialData,
    refreshConfig,
    refreshMcpServers,
    refreshSkills,
    currentProfileId,
    currentSessionId,
    markBackendUnavailable,
    clearBackendUnavailable,
  } = useStore();

  useEffect(() => {
    void loadInitialData();
  }, [loadInitialData]);

  useEffect(() => {
    if (location.pathname === '/settings') {
      void refreshConfig();
      void refreshMcpServers();
      void refreshSkills();
    }
  }, [
    location.pathname,
    refreshConfig,
    refreshMcpServers,
    refreshSkills,
  ]);

  useEffect(() => {
    if (!('serviceWorker' in navigator)) {
      return;
    }

    const processNotificationCallback = async (notificationId: string) => {
      const normalizedId = notificationId.trim();
      if (
        !normalizedId ||
        handledNotificationIdsRef.current.has(normalizedId)
      ) {
        return;
      }
      handledNotificationIdsRef.current.add(normalizedId);

      const store = useStore.getState();
      try {
        const callback = await consumeNotificationCallback(normalizedId);
        const requestedSessionId =
          typeof callback.sessionId === 'string' &&
          callback.sessionId.trim().length > 0
            ? callback.sessionId
            : undefined;
        const callbackMessage =
          typeof callback.callbackMessage === 'string'
            ? callback.callbackMessage.trim()
            : '';
        const callbackContextXml =
          typeof callback.callbackContextXml === 'string'
            ? callback.callbackContextXml.trim()
            : '';
        const hasCallback = callback.hasCallback;
        const callbackConsumed = callback.callbackConsumed;

        let callbackProfileId: string | null =
          typeof callback.profileId === 'string' && callback.profileId.trim().length > 0
            ? callback.profileId
            : null;
        if (!callbackProfileId && requestedSessionId) {
          const knownSession = store.sessions.find(
            (session) => session.id === requestedSessionId,
          );
          callbackProfileId =
            knownSession?.profileId ??
            (await fetchSession(requestedSessionId)).profileId ??
            null;
        }

        if (callbackProfileId && callbackProfileId !== store.currentProfileId) {
          await store.switchProfile(callbackProfileId);
        }

        if (hasCallback && callbackConsumed) {
          return;
        }

        const existingSession = requestedSessionId
          ? store.sessions.find((session) => session.id === requestedSessionId)
          : undefined;
        const targetSessionId = callbackMessage
          ? await store.createSession(undefined, callbackProfileId)
          : (existingSession?.id ??
            (await store.createSession(undefined, callbackProfileId)));

        store.setCurrentSession(targetSessionId);
        navigate(`/chat/${encodeURIComponent(targetSessionId)}`, {
          replace: true,
        });

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
        handledNotificationIdsRef.current.delete(normalizedId);
        const reason = error instanceof Error ? error.message : String(error);
        store.markBackendUnavailable(reason);
        console.error('failed to process notification click callback', error);
      }
    };

    const handleServiceWorkerMessage = (event: MessageEvent) => {
      const data = event.data as
        | {
            type?: string;
            payload?: {
              notificationId?: unknown;
            };
          }
        | undefined;

      if (!data || data.type !== 'may-notification-click') {
        return;
      }

      const notificationId =
        typeof data.payload?.notificationId === 'string'
          ? data.payload.notificationId
          : '';
      if (!notificationId.trim()) {
        return;
      }

      void processNotificationCallback(notificationId);
    };

    navigator.serviceWorker.addEventListener(
      'message',
      handleServiceWorkerMessage,
    );
    const notificationId = new URLSearchParams(location.search).get(
      'notification',
    );
    if (notificationId && notificationId.trim()) {
      void processNotificationCallback(notificationId).finally(() => {
        const params = new URLSearchParams(location.search);
        params.delete('notification');
        const nextSearch = params.toString();
        navigate(
          {
            pathname: location.pathname,
            search: nextSearch ? `?${nextSearch}` : '',
          },
          { replace: true },
        );
      });
    }

    return () => {
      navigator.serviceWorker.removeEventListener(
        'message',
        handleServiceWorkerMessage,
      );
    };
  }, [location.pathname, location.search, navigate]);

  useEffect(() => {
    const isChatRoute = location.pathname.startsWith('/chat');
    const isSettingsRoute = location.pathname === '/settings';
    const unsubscribeEvents = subscribeUiEvents({
      sessionId: isChatRoute ? currentSessionId ?? undefined : undefined,
      profileId: isChatRoute ? currentProfileId ?? undefined : undefined,
      onOpen: () => {},
      onBrowserStatus: isChatRoute
        ? (status) => {
            useStore.getState().setBrowserStatus(status);
          }
        : undefined,
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
              maxIterations: data.maxIterations ?? state.uiConfig.maxIterations,
              fullConfig: nextFullConfig,
            },
          };
        });
      },
      onContainerPrepare: isChatRoute
        ? (payload) => {
            clearBackendUnavailable();
            useStore.getState().setContainerPrepareStatuses(payload.statuses);
          }
        : undefined,
      onRemoteAccess: isSettingsRoute
        ? ({ status }) => {
            useStore.getState().setRemoteAccessStatus(status);
          }
        : undefined,
      onRequestLogs: isSettingsRoute
        ? (payload) => {
            clearBackendUnavailable();
            useStore.getState().setRequestLogs(payload);
          }
        : undefined,
      onNotificationMailbox: isSettingsRoute
        ? (payload) => {
            clearBackendUnavailable();
            useStore.getState().setNotificationMailbox(payload);
          }
        : undefined,
      onNetworkAccessLogs: isSettingsRoute
        ? (payload) => {
            clearBackendUnavailable();
            useStore.getState().setNetworkAccessLogs(payload);
          }
        : undefined,
      onProfileSessions: isChatRoute
        ? ({ profileSessions }) => {
            clearBackendUnavailable();
            useRuntimeStore.getState().applyRuntimes(
              flattenProfileSessions(profileSessions).map((session) => ({
                sessionId: session.id,
                runtime: session.runtime,
              })),
            );
            useStore.setState({
              profileSessions,
              profileSessionsLoading: false,
              profileAccessBlockedReason: null,
              profileAccessBlockedCode: null,
              backendUnavailableReason: null,
            });
          }
        : undefined,
      onSessionRuntimes: isChatRoute
        ? ({ sessions }) => {
            clearBackendUnavailable();
            useRuntimeStore.getState().applyRuntimes(sessions);
          }
        : undefined,
      onSessionRuntime: isChatRoute
        ? ({ sessionId, runtime }) => {
            clearBackendUnavailable();
            useRuntimeStore.getState().applyRuntime(sessionId, runtime);
          }
        : undefined,
      onHeartbeat: () => {},
      onError: () => {
        markBackendUnavailable('Backend event stream disconnected');
      },
    });

    return () => {
      unsubscribeEvents();
    };
  }, [
    clearBackendUnavailable,
    currentProfileId,
    currentSessionId,
    location.pathname,
    markBackendUnavailable,
  ]);

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
