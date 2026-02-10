import React, { useRef, useEffect, useState } from 'react';
import { useStore } from '@/lib/store';
import { MessageItem } from './MessageItem';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Textarea } from '@/components/ui/textarea';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { ModelSelector, getModelDetails } from './ModelSelector';
import { AttachmentPreview } from './AttachmentPreview';
import { Send, Plus, X, File, Pencil, Image as ImageIcon, Monitor, PanelLeft, Share2 } from 'lucide-react';
import type { Attachment } from '@/types';
import { v4 as uuidv4 } from 'uuid';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

interface ChatInterfaceProps {
  onToggleSidebar: () => void;
  isSidebarOpen: boolean;
}

export function ChatInterface({ onToggleSidebar, isSidebarOpen }: ChatInterfaceProps) {
  const { 
    currentSessionId, 
    sessions, 
    addMessage, 
    updateSessionTitle, 
    updateSessionModel,
    updateSessionReasoningEffort 
  } = useStore();
  const [inputValue, setInputValue] = useState('');
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleInput, setTitleInput] = useState('');
  
  const scrollRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const titleInputRef = useRef<HTMLInputElement>(null);

  const currentSession = sessions.find((s) => s.id === currentSessionId);
  const messages = currentSession?.messages || [];

  useEffect(() => {
    if (currentSession) {
      setTitleInput(currentSession.title);
    }
  }, [currentSession?.id, currentSession?.title]);

  useEffect(() => {
    if (isEditingTitle && titleInputRef.current) {
      titleInputRef.current.focus();
    }
  }, [isEditingTitle]);

  // Scroll to bottom on new message
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [messages, attachments]);

  const handleSend = () => {
    if (!currentSessionId || (!inputValue.trim() && attachments.length === 0)) return;

    addMessage(currentSessionId, inputValue, 'user', attachments);
    setInputValue('');
    setAttachments([]);

    // Simulate bot response
    setTimeout(() => {
      addMessage(
        currentSessionId,
        'This is a mock response from the AI assistant.',
        'assistant'
      );
    }, 1000);
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  const handleTitleSubmit = () => {
    if (currentSessionId && titleInput.trim()) {
      updateSessionTitle(currentSessionId, titleInput.trim());
    }
    setIsEditingTitle(false);
  };

  const handleTitleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      handleTitleSubmit();
    } else if (e.key === 'Escape') {
      setTitleInput(currentSession?.title || '');
      setIsEditingTitle(false);
    }
  };

  const handleScreenshot = async () => {
    try {
      const stream = await navigator.mediaDevices.getDisplayMedia({
        video: {
          displaySurface: "browser",
        },
        audio: false,
        preferCurrentTab: true,
      } as DisplayMediaStreamOptions); // Cast to any if type definition is outdated
      
      const video = document.createElement('video');
      video.srcObject = stream;
      
      // Wait for video to load metadata and play
      await new Promise((resolve) => {
        video.onloadedmetadata = () => {
          video.play().then(resolve);
        };
      });
      
      // Create canvas and capture frame
      const canvas = document.createElement('canvas');
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      const ctx = canvas.getContext('2d');
      ctx?.drawImage(video, 0, 0);
      
      // Stop all tracks to release resources
      stream.getTracks().forEach(track => track.stop());
      
      const screenshotUrl = canvas.toDataURL('image/png');
      
      const screenshot: Attachment = {
        id: uuidv4(),
        name: `screenshot-${new Date().toISOString()}.png`,
        type: 'image',
        url: screenshotUrl
      };
      setAttachments(prev => [...prev, screenshot]);
    } catch (error) {
      console.error('Screenshot failed:', error);
    }
  };

  const handleFileSelect = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files.length > 0) {
      const newAttachments: Attachment[] = Array.from(e.target.files).map(file => ({
        id: uuidv4(),
        name: file.name,
        type: file.type.startsWith('image/') ? 'image' : 'file',
        url: URL.createObjectURL(file)
      }));
      setAttachments(prev => [...prev, ...newAttachments]);
    }
    // Reset input
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  const removeAttachment = (id: string) => {
    setAttachments(prev => prev.filter(a => a.id !== id));
  };

  if (!currentSessionId || !currentSession) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-muted-foreground">
        <p>Select a chat to start messaging</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-screen max-w-4xl mx-auto w-full relative">
      {/* Header */}
      <div className="flex items-center justify-between p-4 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60 z-10">
        <div className="flex-1 flex justify-start">
          <Button
            variant="ghost"
            size="icon"
            onClick={onToggleSidebar}
            className="text-muted-foreground hover:text-foreground hidden md:flex"
            title={isSidebarOpen ? "Close sidebar" : "Open sidebar"}
          >
            <PanelLeft className="h-5 w-5" />
          </Button>
        </div>
        <div className="flex items-center gap-2 min-w-0">
           {isEditingTitle ? (
            <Input
              ref={titleInputRef}
              value={titleInput}
              onChange={(e) => setTitleInput(e.target.value)}
              onBlur={handleTitleSubmit}
              onKeyDown={handleTitleKeyDown}
              className="h-auto py-0 w-[300px] text-sm font-medium text-foreground text-center bg-transparent border-none focus-visible:ring-0 px-0"
            />
          ) : (
            <div 
              className="flex items-center gap-2 group cursor-pointer min-w-0"
              onClick={() => setIsEditingTitle(true)}
            >
              <span className="text-sm font-medium text-foreground truncate max-w-[300px]">
                {currentSession.title}
              </span>
              <Pencil className="h-3 w-3 text-muted-foreground opacity-0 group-hover:opacity-100 transition-opacity" />
            </div>
          )}
        </div>
        <div className="flex-1 flex justify-end">
          <Button
            variant="ghost"
            size="icon"
            className="text-muted-foreground hover:text-foreground"
            title="Share chat"
          >
            <Share2 className="h-5 w-5" />
          </Button>
        </div>
      </div>

      <ScrollArea className="flex-1 px-4">
        <div className="space-y-6 pb-[200px]">
          {messages.map((message) => (
            <MessageItem key={message.id} message={message} />
          ))}
          <div ref={scrollRef} />
        </div>
      </ScrollArea>

      <div className="p-4 bg-transparent absolute bottom-0 left-0 right-0 w-full z-20 pointer-events-none">
        <div className="w-full pointer-events-auto">
          <div className="relative bg-background/40 backdrop-blur-xl rounded-2xl border border-white/20 shadow-lg transition-all">
            {/* Attachment Previews */}
            {attachments.length > 0 && (
              <div className="flex gap-2 p-3 pb-0 overflow-x-auto">
                {attachments.map((att) => (
                  <div key={att.id} className="relative group flex-shrink-0">
                    <AttachmentPreview attachment={att}>
                      <div className="h-16 w-16 border rounded-md overflow-hidden bg-muted flex items-center justify-center">
                        {att.type === 'image' ? (
                          <img src={att.url} alt={att.name} className="h-full w-full object-cover" />
                        ) : (
                          <File className="h-8 w-8 text-muted-foreground" />
                        )}
                      </div>
                    </AttachmentPreview>
                    <button
                      onClick={(e) => {
                        e.stopPropagation(); // Prevent preview triggering
                        removeAttachment(att.id);
                      }}
                      className="absolute -top-1 -right-1 bg-destructive text-destructive-foreground rounded-full p-0.5 shadow-sm opacity-0 group-hover:opacity-100 transition-opacity z-10"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </div>
                ))}
              </div>
            )}

            <Textarea
              value={inputValue}
              onChange={(e) => setInputValue(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Type a message..."
              className="min-h-[60px] max-h-[200px] w-full resize-none border-0 shadow-none focus-visible:ring-0 bg-transparent py-4 px-3 text-base"
              rows={1}
            />

            <div className="flex items-center justify-between p-2 pl-3">
              <div className="flex items-center gap-2">
                <input
                  type="file"
                  multiple
                  className="hidden"
                  ref={fileInputRef}
                  onChange={handleFileSelect}
                />
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button 
                      variant="ghost" 
                      size="icon" 
                      className="h-8 w-8 text-muted-foreground hover:text-foreground rounded-lg"
                    >
                      <Plus className="h-5 w-5" />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="start" className="w-48">
                    <DropdownMenuItem onClick={() => fileInputRef.current?.click()}>
                      <File className="mr-2 h-4 w-4" />
                      Upload File
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => fileInputRef.current?.click()}>
                      <ImageIcon className="mr-2 h-4 w-4" />
                      Upload Photo
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={handleScreenshot}>
                      <Monitor className="mr-2 h-4 w-4" />
                      Take Screenshot
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>

              <div className="flex items-center gap-2">
                {(() => {
                  const model = getModelDetails(currentSession.model);
                  const ProviderIcon = model.icon;
                  return (
                    <ModelSelector 
                      currentModelId={currentSession.model}
                      currentReasoningEffort={currentSession.reasoningEffort}
                      onModelChange={(modelId) => updateSessionModel(currentSessionId, modelId)}
                      onReasoningEffortChange={(effort) => updateSessionReasoningEffort(currentSessionId, effort)}
                      trigger={
                        <Button 
                          variant="ghost" 
                          size="sm" 
                          className="h-8 gap-2 text-xs text-muted-foreground hover:text-foreground font-medium px-2"
                        >
                          <ProviderIcon className="h-4 w-4" />
                          <span className="truncate">{model.name}</span>

                        </Button>
                      }
                    />
                  );
                })()}
                <Button 
                  onClick={handleSend}
                  disabled={!inputValue.trim() && attachments.length === 0}
                  size="icon"
                  className={inputValue.trim() || attachments.length > 0 ? "h-8 w-8 bg-primary text-primary-foreground rounded-lg" : "h-8 w-8 bg-muted text-muted-foreground rounded-lg"}
                >
                  <Send className="h-4 w-4" />
                </Button>
              </div>
            </div>
          </div>
          <div className="text-xs text-center text-muted-foreground mt-3">
            AI can make mistakes. Check important info.
          </div>
        </div>
      </div>
    </div>
  );
}
