import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowUp, Paperclip, Square, X, Zap, ListOrdered } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import { useTranslation } from '@/lib/i18n';
import type { ApiAttachment, AttachmentKind } from '@/lib/types';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { ModelPicker } from './ModelPicker';
import { ThinkingPicker } from './ThinkingPicker';

interface ComposerProps {
  disabled?: boolean;
  streaming?: boolean;
  onSend: (text: string, attachments: ApiAttachment[], deliveryMode: 'steer' | 'queue') => void;
  onCancel: () => void;
  sessionId: string;
  currentModel: string;
  currentReasoningEffort: string | null | undefined;
}

function detectKind(mime: string): AttachmentKind {
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('audio/')) return 'audio';
  if (mime.startsWith('video/')) return 'video';
  if (mime.startsWith('text/') || mime === 'application/json') return 'text';
  if (mime === 'application/pdf') return 'pdf';
  if (mime.startsWith('application/')) return 'document';
  return 'file';
}

async function fileToAttachment(file: File): Promise<ApiAttachment> {
  const kind = detectKind(file.type || '');
  const reader = new FileReader();
  const dataUrl = await new Promise<string>((resolve, reject) => {
    reader.onerror = () => reject(reader.error);
    reader.onload = () => resolve(reader.result as string);
    reader.readAsDataURL(file);
  });
  let textContent: string | null = null;
  if (kind === 'text' && file.size < 256 * 1024) {
    textContent = await file.text();
  }
  return {
    id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`,
    name: file.name,
    type: kind,
    url: dataUrl,
    uploadDataUrl: dataUrl,
    textContent,
  };
}

export function Composer({
  disabled,
  streaming,
  onSend,
  onCancel,
  sessionId,
  currentModel,
  currentReasoningEffort,
}: ComposerProps) {
  const [value, setValue] = useState('');
  const [attachments, setAttachments] = useState<ApiAttachment[]>([]);
  const [deliveryMode, setDeliveryMode] = useState<'steer' | 'queue'>('steer');
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const { t } = useTranslation();

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = '0px';
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [value]);

  const send = useCallback(() => {
    const trimmed = value.trim();
    if (!trimmed && attachments.length === 0) return;
    if (disabled) return;
    onSend(trimmed, attachments, deliveryMode);
    setValue('');
    setAttachments([]);
    setDeliveryMode('steer');
  }, [value, attachments, deliveryMode, disabled, onSend]);

  const addFiles = async (files: FileList | File[]) => {
    const list = Array.from(files);
    const mapped = await Promise.all(list.map(fileToAttachment));
    setAttachments((prev) => [...prev, ...mapped]);
  };

  const handlePaste = async (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const items = e.clipboardData.items;
    const files: File[] = [];
    for (const item of items) {
      if (item.kind === 'file') {
        const file = item.getAsFile();
        if (file) files.push(file);
      }
    }
    if (files.length > 0) {
      e.preventDefault();
      await addFiles(files);
    }
  };

  const handleDrop = async (e: React.DragEvent<HTMLDivElement>) => {
    if (e.dataTransfer.files.length === 0) return;
    e.preventDefault();
    await addFiles(e.dataTransfer.files);
  };

  return (
    <div
      className="rounded-lg border border-border bg-background shadow-[0_2px_8px_-2px_rgb(0_0_0/0.08)] dark:shadow-none"
      onDragOver={(e) => e.preventDefault()}
      onDrop={handleDrop}
    >
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-1.5 border-b border-border px-3 py-2">
          {attachments.map((a) => (
            <span
              key={a.id}
              className="inline-flex items-center gap-1.5 rounded-md border border-border bg-muted/40 px-2 py-1 text-xs"
            >
              <Paperclip className="size-3" />
              <span className="max-w-[160px] truncate">{a.name}</span>
              <button
                type="button"
                onClick={() =>
                  setAttachments((prev) => prev.filter((x) => x.id !== a.id))
                }
                className="text-muted-foreground hover:text-foreground"
                aria-label={t('common.remove')}
              >
                <X className="size-3" />
              </button>
            </span>
          ))}
        </div>
      )}
      <textarea
        ref={textareaRef}
        className="block w-full resize-none bg-transparent px-3 pt-3 pb-1 text-sm outline-none placeholder:text-muted-foreground min-h-[48px] max-h-[240px] scrollbar-thin"
        placeholder={streaming ? t('composer.placeholderBusy') : t('composer.placeholder')}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onPaste={handlePaste}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            send();
          }
        }}
      />
      <div className="flex items-center justify-between px-2 pb-2">
        <div className="flex items-center gap-0.5">
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            onChange={async (e) => {
              if (!e.target.files) return;
              await addFiles(e.target.files);
              e.target.value = '';
            }}
          />
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                size="icon"
                variant="ghost"
                type="button"
                onClick={() => fileInputRef.current?.click()}
                aria-label={t('composer.attach')}
              >
                <Paperclip className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>{t('composer.attach')}</TooltipContent>
          </Tooltip>
          {streaming && (
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  size="icon"
                  variant={deliveryMode === 'queue' ? 'secondary' : 'ghost'}
                  type="button"
                  onClick={() =>
                    setDeliveryMode((m) => (m === 'queue' ? 'steer' : 'queue'))
                  }
                  aria-label={t('composer.queued')}
                >
                  {deliveryMode === 'queue' ? (
                    <ListOrdered className="size-4" />
                  ) : (
                    <Zap className="size-4" />
                  )}
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                {deliveryMode === 'queue' ? t('composer.queueHint') : t('composer.immediateHint')}
              </TooltipContent>
            </Tooltip>
          )}
          <div className="mx-1 h-4 w-px bg-border" />
          <ModelPicker
            sessionId={sessionId}
            currentModel={currentModel}
            disabled={disabled}
          />
          <ThinkingPicker
            sessionId={sessionId}
            current={currentReasoningEffort}
            disabled={disabled}
          />
        </div>
        {streaming ? (
          <Button size="icon" variant="outline" onClick={onCancel} aria-label={t('composer.stop')}>
            <Square className="size-3.5 fill-current" />
          </Button>
        ) : (
          <Button
            size="icon"
            onClick={send}
            disabled={disabled || (value.trim().length === 0 && attachments.length === 0)}
            aria-label={t('composer.send')}
            className={cn(
              'transition-opacity',
              value.trim().length === 0 && attachments.length === 0 && 'opacity-50',
            )}
          >
            <ArrowUp className="size-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
