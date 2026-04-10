import type { ChangeEvent, ClipboardEvent, RefObject } from 'react';
import { File, Image as ImageIcon, Monitor, Plus, Send, Square, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { AttachmentCard } from './AttachmentCard';
import { AttachmentPreview } from './AttachmentPreview';
import { ModelSelector } from './ModelSelector';
import { ReasoningEffortSelector } from './ReasoningEffortSelector';
import { ContextInspector } from './ContextInspectorDialog';
import type { Attachment } from '@/types';

interface ComposerProps {
  inputValue: string;
  onInputChange: (value: string) => void;
  placeholder: string;
  attachments: Attachment[];
  onRemoveAttachment: (id: string) => void;
  onPaste: (event: ClipboardEvent) => void;
  onFileSelect: (event: ChangeEvent<HTMLInputElement>) => void;
  fileInputRef: RefObject<HTMLInputElement | null>;
  composerInputRef: RefObject<HTMLTextAreaElement | null>;
  isNewChat: boolean;
  canSend: boolean;
  onSend: () => void;
  onScreenshot: () => void;

  currentModelId: string;
  onModelChange: (id: string) => void;
  isModelUpdating: boolean;
  isSending: boolean;
  currentModelSupportsReasoning: boolean;
  currentReasoningEffort: string | undefined;
  onReasoningEffortChange: (effort: string | undefined) => void;

  showTokenUsage: boolean;
  currentSessionId: string | null;
  promptTokens?: number;
  contextUsedTokens?: number;
  contextWindow?: number;
  contextUsagePercent: number | null;
  contextUsageTitle: string;

  showStopButton: boolean;
  showSendButton: boolean;
  onStop: () => void;
}

export function Composer({
  inputValue,
  onInputChange,
  placeholder,
  attachments,
  onRemoveAttachment,
  onPaste,
  onFileSelect,
  fileInputRef,
  composerInputRef,
  isNewChat,
  canSend,
  onSend,
  onScreenshot,
  currentModelId,
  onModelChange,
  isModelUpdating,
  isSending,
  currentModelSupportsReasoning,
  currentReasoningEffort,
  onReasoningEffortChange,
  showTokenUsage,
  currentSessionId,
  promptTokens,
  contextUsedTokens,
  contextWindow,
  contextUsagePercent,
  contextUsageTitle,
  showStopButton,
  showSendButton,
  onStop,
}: ComposerProps) {
  return (
    <div className="relative rounded-2xl border border-border/50 bg-background/60 backdrop-blur-xl">
      {attachments.length > 0 && (
        <div className="flex gap-2 p-3 pb-0 overflow-x-auto">
          {attachments.map((att) => (
            <div key={att.id} className="relative group flex-shrink-0">
              <AttachmentPreview attachment={att}>
                <AttachmentCard attachment={att} variant="composer" />
              </AttachmentPreview>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onRemoveAttachment(att.id);
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
        ref={composerInputRef}
        value={inputValue}
        onChange={(e) => onInputChange(e.target.value)}
        onPaste={onPaste}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            if (!canSend) return;
            onSend();
          }
        }}
        placeholder={placeholder}
        className="min-h-[60px] max-h-[200px] w-full resize-none border-0 bg-transparent px-3 py-4 text-base shadow-none focus-visible:ring-0"
        rows={1}
        autoFocus={isNewChat}
      />

      <div className="flex items-center justify-between p-2 pl-3">
        <div className="flex items-center gap-2">
          <input
            type="file"
            multiple
            className="hidden"
            ref={fileInputRef}
            onChange={onFileSelect}
          />
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="h-8 w-8 rounded-lg text-muted-foreground hover:text-foreground"
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
              <DropdownMenuItem onClick={onScreenshot}>
                <Monitor className="mr-2 h-4 w-4" />
                Take Screenshot
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
        <div className="flex items-center gap-2">
          <ModelSelector
            currentModelId={currentModelId}
            onModelChange={onModelChange}
            disabled={isModelUpdating || isSending}
            isUpdating={isModelUpdating}
          />
          {currentModelSupportsReasoning && (
            <ReasoningEffortSelector
              value={currentReasoningEffort}
              onChange={onReasoningEffortChange}
              disabled={isModelUpdating || isSending}
            />
          )}
          {showTokenUsage && (
            <ContextInspector
              currentSessionId={currentSessionId}
              promptTokens={promptTokens}
              contextUsedTokens={contextUsedTokens}
              contextWindow={contextWindow}
              contextUsagePercent={contextUsagePercent}
              contextUsageTitle={contextUsageTitle}
            />
          )}
          {showStopButton && (
            <Button
              onClick={onStop}
              size="icon"
              variant="ghost"
              className="h-8 w-8 rounded-lg text-destructive hover:bg-destructive/10 hover:text-destructive"
            >
              <Square className="h-3.5 w-3.5 fill-current" />
            </Button>
          )}
          {showSendButton && (
            <Button
              onClick={onSend}
              disabled={!canSend}
              size="icon"
              className={
                canSend
                  ? 'h-8 w-8 rounded-lg bg-primary text-primary-foreground'
                  : 'h-8 w-8 rounded-lg bg-muted text-muted-foreground'
              }
            >
              <Send className="h-4 w-4" />
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}
