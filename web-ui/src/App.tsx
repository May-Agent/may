import { useState, useEffect } from 'react';
import { Sidebar } from '@/components/layout/Sidebar';
import { SettingsPage } from '@/components/layout/SettingsPage';
import { ChatInterface } from '@/components/chat/ChatInterface';
import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';

function App() {
  const [isSidebarOpen, setSidebarOpen] = useState(false);
  const [isDesktopSidebarOpen, setDesktopSidebarOpen] = useState(true);
  const [showSettingsPage, setShowSettingsPage] = useState(false);
  const {
    settings,
    loadInitialData,
    refreshRuntimeSettings,
    refreshFullConfig,
    refreshMcpServers,
    refreshSkills,
  } = useStore();

  useEffect(() => {
    void loadInitialData();
  }, [loadInitialData]);

  useEffect(() => {
    if (showSettingsPage) {
      void refreshRuntimeSettings();
      void refreshFullConfig();
      void refreshMcpServers();
      void refreshSkills();
    }
  }, [showSettingsPage, refreshRuntimeSettings, refreshFullConfig, refreshMcpServers, refreshSkills]);

  useEffect(() => {
    if (settings.font && !settings.font.startsWith('Source Han')) {
      const linkId = 'google-font-link';
      let link = document.getElementById(linkId) as HTMLLinkElement;
      if (!link) { link = document.createElement('link'); link.id = linkId; link.rel = 'stylesheet'; document.head.appendChild(link); }
      link.href = `https://fonts.googleapis.com/css2?family=${settings.font.replace(/\s+/g, '+')}:wght@400;500;600;700&display=swap`;
    }

    const root = document.documentElement;
    root.classList.remove('light', 'dark');
    root.classList.add(settings.theme === 'system' ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') : settings.theme);
  }, [settings.theme, settings.font]);

  return (
    <div className="flex h-screen bg-background text-foreground overflow-hidden" style={{ fontFamily: FONTS.find(f => f.value === settings.font)?.family }}>
      <div className="md:hidden fixed top-4 left-4 z-50">
        <Sheet open={isSidebarOpen} onOpenChange={setSidebarOpen}>
          <SheetTrigger asChild><Button variant="outline" size="icon"><Menu className="h-4 w-4" /></Button></SheetTrigger>
          <SheetContent side="left" className="p-0 w-72">
            <Sidebar onOpenSettings={() => setShowSettingsPage(true)} />
          </SheetContent>
        </Sheet>
      </div>

      <div className={`${isDesktopSidebarOpen ? 'w-64' : 'w-0'} hidden md:block h-full transition-all duration-300 ease-in-out overflow-hidden border-r`}>
        <Sidebar className="w-64" onOpenSettings={() => setShowSettingsPage(true)} />
      </div>

      <main className="flex-1 flex flex-col h-full relative">
        {showSettingsPage ? (
          <SettingsPage onClose={() => setShowSettingsPage(false)} />
        ) : (
          <ChatInterface onToggleSidebar={() => setDesktopSidebarOpen(!isDesktopSidebarOpen)} isSidebarOpen={isDesktopSidebarOpen} />
        )}
      </main>
    </div>
  );
}

export default App;
