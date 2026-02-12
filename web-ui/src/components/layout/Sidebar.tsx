import { useState } from 'react';
import { useStore } from '@/lib/store';
import { useRuntimeStore } from '@/lib/runtimeStore';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Search, Plus, Trash2, Settings } from 'lucide-react';

interface SidebarProps {
  className?: string;
  onOpenSettings: () => void;
  onSelectSession?: (sessionId: string) => void;
  onNewChat?: () => void;
}

export function Sidebar({ className, onOpenSettings, onSelectSession, onNewChat }: SidebarProps) {
  const {
    sessions,
    currentSessionId,
    setCurrentSession,
    deleteSession,
    searchSessions,
    loaded,
  } = useStore();
  const runtimeSessions = useRuntimeStore((state) => state.sessions);

  const [searchQuery, setSearchQuery] = useState('');

  const filteredSessions = searchQuery
    ? searchSessions(searchQuery)
    : sessions;

  return (
    <div className={cn("w-64 bg-muted/20 flex flex-col h-screen", className)}>
        <div className="flex-1 flex flex-col min-h-0">
          <div className="px-3 py-4 space-y-4">
            <Button 
              onClick={() => {
                setCurrentSession(null);
                onNewChat?.();
              }} 
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
                {!loaded && (
                  <div className="space-y-2 px-3 py-2">
                    {Array.from({ length: 6 }).map((_, i) => (
                      <div key={i} className="h-8 rounded bg-muted/50 animate-pulse" />
                    ))}
                  </div>
                )}

                {loaded && filteredSessions.map((session) => (
                  <div
                    key={session.id}
                    className={cn(
                      "group flex items-center justify-between rounded-md px-3 py-2 text-sm font-medium hover:bg-accent hover:text-accent-foreground cursor-pointer transition-colors",
                      currentSessionId === session.id ? "bg-accent text-accent-foreground" : "transparent"
                    )}
                    onClick={() => {
                      setCurrentSession(session.id);
                      onSelectSession?.(session.id);
                    }}
                  >
                    <div className="flex items-center truncate flex-1 min-w-0 gap-2">
                      {runtimeSessions[session.id]?.state === 'streaming' && (
                        <span className="flex gap-0.5" aria-label="running session">
                          <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse" />
                          <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse [animation-delay:150ms]" />
                          <span className="h-1.5 w-1.5 rounded-full bg-foreground/45 animate-pulse [animation-delay:300ms]" />
                        </span>
                      )}
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
                
                {loaded && searchQuery.trim().length > 0 && filteredSessions.length === 0 && (
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
            onClick={onOpenSettings}
          >
            <Settings className="h-4 w-4" />
            Settings
          </Button>
        </div>
      </div>
  );
}
