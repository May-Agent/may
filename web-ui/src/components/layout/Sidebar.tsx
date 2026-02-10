import { useState } from 'react';
import { useStore } from '@/lib/store';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { ScrollArea } from '@/components/ui/scroll-area';
import { SettingsDialog } from '@/components/settings/SettingsDialog';
import {
  MessageSquare,
  Search,
  Plus,
  Trash2,
  Settings,
} from 'lucide-react';

interface SidebarProps {
  className?: string;
}

export function Sidebar({ className }: SidebarProps) {
  const { 
    sessions, 
    currentSessionId, 
    setCurrentSession, 
    createSession, 
    deleteSession,
    searchSessions 
  } = useStore();
  
  const [searchQuery, setSearchQuery] = useState('');
  const [isSettingsOpen, setIsSettingsOpen] = useState(false);

  const filteredSessions = searchQuery 
    ? searchSessions(searchQuery)
    : sessions;

  return (
    <>
      <div className={cn("w-64 bg-muted/20 flex flex-col h-screen", className)}>
        <div className="flex-1 flex flex-col min-h-0">
          <div className="px-3 py-4 space-y-4">
            <Button 
              onClick={() => createSession()} 
              variant="outline" 
              className="w-full justify-start gap-2"
            >
              <Plus className="h-4 w-4" />
              New Chat
            </Button>
            
            <div className="relative">
              <Search className="absolute left-2 top-2.5 h-4 w-4 text-muted-foreground" />
              <Input 
                placeholder="Search history..." 
                className="pl-8" 
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
              />
            </div>
          </div>
          
          <div className="flex-1 flex flex-col min-h-0 px-3">
            <h2 className="mb-2 px-4 text-xs font-semibold text-muted-foreground uppercase tracking-wider">
              History
            </h2>
            <ScrollArea className="flex-1">
              <div className="space-y-1 pr-3">
                {filteredSessions.map((session) => (
                  <div
                    key={session.id}
                    className={cn(
                      "group flex items-center justify-between rounded-md px-3 py-2 text-sm font-medium hover:bg-accent hover:text-accent-foreground cursor-pointer transition-colors",
                      currentSessionId === session.id ? "bg-accent text-accent-foreground" : "transparent"
                    )}
                    onClick={() => setCurrentSession(session.id)}
                  >
                    <div className="flex items-center gap-2 truncate flex-1 min-w-0">
                      <MessageSquare className="h-4 w-4 flex-shrink-0" />
                      <span className="truncate">{session.title || "New Chat"}</span>
                    </div>
                    
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6 flex-shrink-0 opacity-0 group-hover:opacity-100 transition-opacity"
                      onClick={(e) => {
                        e.stopPropagation();
                        deleteSession(session.id);
                      }}
                    >
                      <Trash2 className="h-3 w-3 text-muted-foreground hover:text-destructive" />
                    </Button>
                  </div>
                ))}
                
                {filteredSessions.length === 0 && (
                  <div className="text-center text-sm text-muted-foreground py-8">
                    No chats found
                  </div>
                )}
              </div>
            </ScrollArea>
          </div>
        </div>
        
        <div className="mt-auto p-4 border-t bg-background">
          <Button 
            variant="ghost" 
            className="w-full justify-start gap-2"
            onClick={() => setIsSettingsOpen(true)}
          >
            <Settings className="h-4 w-4" />
            Settings
          </Button>
        </div>
      </div>
      <SettingsDialog open={isSettingsOpen} onOpenChange={setIsSettingsOpen} />
    </>
  );
}
