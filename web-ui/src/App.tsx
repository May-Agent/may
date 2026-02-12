import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { Navigate, Route, Routes, useLocation, useNavigate, useParams } from 'react-router-dom';
import { Sidebar } from '@/components/layout/Sidebar';
import { SettingsPage } from '@/components/layout/SettingsPage';
import { ChatInterface } from '@/components/chat/ChatInterface';
import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';

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
    <div className="flex h-screen bg-background text-foreground overflow-hidden">
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

      <div className={`${isDesktopSidebarOpen ? 'w-64' : 'w-0'} hidden md:block h-full transition-all duration-300 ease-in-out overflow-hidden border-r`}>
        <Sidebar
          className="w-64"
          onOpenSettings={onOpenSettings}
          onSelectSession={(id) => navigate(`/chat/${encodeURIComponent(id)}`)}
          onNewChat={() => navigate('/chat/new')}
        />
      </div>

      <main className="flex-1 flex flex-col h-full relative">{children}</main>
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
  const { loaded, sessions, currentSessionId, setCurrentSession } = useStore();

  useEffect(() => {
    if (!sessionId || sessionId === 'new') {
      if (currentSessionId !== null) {
        setCurrentSession(null);
      }
      return;
    }

    if (!loaded) return;

    const exists = sessions.some((s) => s.id === sessionId);
    if (!exists) {
      if (currentSessionId !== null) {
        setCurrentSession(null);
      }
      return;
    }

    if (currentSessionId !== sessionId) {
      setCurrentSession(sessionId);
    }
  }, [loaded, sessionId, sessions, currentSessionId, setCurrentSession]);

  return (
    <AppScaffold
      isSidebarOpen={isSidebarOpen}
      setSidebarOpen={setSidebarOpen}
      isDesktopSidebarOpen={isDesktopSidebarOpen}
      onOpenSettings={onOpenSettings}
    >
      <ChatInterface
        onToggleSidebar={() => setDesktopSidebarOpen(!isDesktopSidebarOpen)}
        isSidebarOpen={isDesktopSidebarOpen}
      />
    </AppScaffold>
  );
}

function SettingsRouteView() {
  return (
    <div className="h-screen bg-background text-foreground overflow-auto">
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
    updateSettings,
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
    let source: EventSource | null = null;
    let reconnectTimer: number | null = null;

    const setReconnect = (reconnecting: boolean) => {
      const id = 'config-reconnect-banner';
      const exists = document.getElementById(id);
      if (reconnecting) {
        if (!exists) {
          const div = document.createElement('div');
          div.id = id;
          div.textContent = 'Reconnecting to config stream...';
          div.style.cssText =
            'position:fixed;bottom:12px;right:12px;z-index:9999;background:#111827;color:#f9fafb;padding:6px 10px;border-radius:8px;font-size:12px;opacity:0.95';
          document.body.appendChild(div);
        }
      } else if (exists) {
        exists.remove();
      }
    };

    const connect = () => {
      source = new EventSource('/api/config/stream');

      source.addEventListener('config', (ev) => {
        try {
          const data = JSON.parse((ev as MessageEvent).data) as {
            ui_font?: string;
            ui_theme?: 'light' | 'dark' | 'system';
          };
          useStore.getState().updateSettings({
            font: data.ui_font ?? useStore.getState().settings.font,
            theme: data.ui_theme ?? useStore.getState().settings.theme,
          });
          setReconnect(false);
        } catch {
          // ignore malformed event
        }
      });

      source.addEventListener('heartbeat', () => {
        setReconnect(false);
      });

      source.onerror = () => {
        setReconnect(true);
        source?.close();
        source = null;
        if (reconnectTimer == null) {
          reconnectTimer = window.setTimeout(() => {
            reconnectTimer = null;
            connect();
          }, 1200);
        }
      };
    };

    connect();

    return () => {
      if (reconnectTimer != null) {
        window.clearTimeout(reconnectTimer);
      }
      source?.close();
      setReconnect(false);
    };
  }, [updateSettings]);

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
    <div style={{ fontFamily }}>
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
