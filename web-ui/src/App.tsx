// src/App.tsx
import { useState, useEffect } from 'react';
import { Sidebar } from '@/components/layout/Sidebar';
import { ChatInterface } from '@/components/chat/ChatInterface';
import { Menu } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetTrigger } from '@/components/ui/sheet';
import { useStore } from '@/lib/store';

const FONTS = [
  { name: 'Inter', value: 'Inter', family: 'Inter, sans-serif' },
  { name: 'Roboto', value: 'Roboto', family: 'Roboto, sans-serif' },
  { name: 'Open Sans', value: 'Open Sans', family: '"Open Sans", sans-serif' },
  { name: 'Lato', value: 'Lato', family: 'Lato, sans-serif' },
  { name: 'Montserrat', value: 'Montserrat', family: 'Montserrat, sans-serif' },
  { name: 'Source Han Sans', value: 'Source Han Sans SC', family: '"Source Han Sans SC", "Noto Sans SC", sans-serif' },
  { name: 'Source Han Serif', value: 'Source Han Serif SC', family: '"Source Han Serif SC", "Noto Serif SC", serif' },
];

function App() {
  const [isSidebarOpen, setSidebarOpen] = useState(false);
  const [isDesktopSidebarOpen, setDesktopSidebarOpen] = useState(true);
  const { settings } = useStore();

  useEffect(() => {
    // Dynamically load Google Fonts
    if (settings.font && !['Source Han Sans SC', 'Source Han Serif SC'].includes(settings.font)) {
      const linkId = 'google-font-link';
      let link = document.getElementById(linkId) as HTMLLinkElement;
      
      if (!link) {
        link = document.createElement('link');
        link.id = linkId;
        link.rel = 'stylesheet';
        document.head.appendChild(link);
      }
      
      const fontName = settings.font.replace(/\s+/g, '+');
      link.href = `https://fonts.googleapis.com/css2?family=${fontName}:wght@400;500;600;700&display=swap`;
    }
    
    // Apply theme
    const root = window.document.documentElement;
    root.classList.remove('light', 'dark');

    if (settings.theme === 'system') {
      const systemTheme = window.matchMedia('(prefers-color-scheme: dark)').matches
        ? 'dark'
        : 'light';
      root.classList.add(systemTheme);
    } else {
      root.classList.add(settings.theme);
    }
  }, [settings.theme, settings.font]);

  const currentFontFamily = FONTS.find(f => f.value === settings.font)?.family;

  return (
    <div 
      className="flex h-screen bg-background text-foreground overflow-hidden"
      style={{ fontFamily: currentFontFamily }}
    >
      {/* Mobile Sidebar Trigger */}
      <div className="md:hidden fixed top-4 left-4 z-50">
        <Sheet open={isSidebarOpen} onOpenChange={setSidebarOpen}>
          <SheetTrigger asChild>
            <Button variant="outline" size="icon">
              <Menu className="h-4 w-4" />
            </Button>
          </SheetTrigger>
          <SheetContent side="left" className="p-0 w-72">
            <Sidebar />
          </SheetContent>
        </Sheet>
      </div>

      {/* Desktop Sidebar */}
      <div 
        className={`${isDesktopSidebarOpen ? 'w-64' : 'w-0'} hidden md:block h-full transition-all duration-300 ease-in-out overflow-hidden border-r`}
      >
        <Sidebar className="w-64" />
      </div>

      {/* Main Content */}
      <main className="flex-1 flex flex-col h-full relative">
        <ChatInterface 
          onToggleSidebar={() => setDesktopSidebarOpen(!isDesktopSidebarOpen)}
          isSidebarOpen={isDesktopSidebarOpen}
        />
      </main>
    </div>
  );
}

export default App;
