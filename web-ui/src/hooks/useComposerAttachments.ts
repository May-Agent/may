import type { ChangeEvent, ClipboardEvent, Dispatch, RefObject, SetStateAction } from 'react';
import { useCallback } from 'react';
import {
  buildAttachmentFromFile,
  buildAttachmentFromPastedFile,
  buildAttachmentFromPastedText,
} from '@/lib/attachments';
import { LONG_TEXT_THRESHOLD } from '@/components/chat/chatUtils';
import type { Attachment } from '@/types';

export interface UseComposerAttachmentsInput {
  attachments: Attachment[];
  setAttachments: Dispatch<SetStateAction<Attachment[]>>;
  setError: Dispatch<SetStateAction<string | null>>;
  fileInputRef: RefObject<HTMLInputElement | null>;
}

export interface UseComposerAttachmentsResult {
  removeAttachment: (id: string) => void;
  onFileSelect: (event: ChangeEvent<HTMLInputElement>) => Promise<void>;
  onPaste: (event: ClipboardEvent) => Promise<void>;
  takeScreenshot: () => Promise<void>;
}

/**
 * Composer-scoped attachment handlers: file select, paste, screenshot
 * capture, and single-attachment removal. Each handler appends to the
 * shared `attachments` state and surfaces errors through `setError`.
 *
 * The screenshot capture uses a try/finally to release the display
 * media stream even if rendering fails after the stream starts.
 */
export function useComposerAttachments({
  setAttachments,
  setError,
  fileInputRef,
}: UseComposerAttachmentsInput): UseComposerAttachmentsResult {
  const removeAttachment = useCallback(
    (id: string): void => {
      setAttachments((prev) => prev.filter((a) => a.id !== id));
    },
    [setAttachments],
  );

  const onFileSelect = useCallback(
    async (event: ChangeEvent<HTMLInputElement>): Promise<void> => {
      if (!event.target.files?.length) return;
      try {
        const files = Array.from(event.target.files);
        const newAttachments = await Promise.all(
          files.map(buildAttachmentFromFile),
        );
        setAttachments((prev) => [...prev, ...newAttachments]);
        if (fileInputRef.current) fileInputRef.current.value = '';
      } catch (error) {
        setError(
          `Attachment processing failed: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      }
    },
    [fileInputRef, setAttachments, setError],
  );

  const onPaste = useCallback(
    async (event: ClipboardEvent): Promise<void> => {
      const items = Array.from(event.clipboardData.items);
      const text = event.clipboardData.getData('text/plain');
      const hasFileItem = items.some(
        (item) => item.kind === 'file' && item.getAsFile(),
      );
      const shouldAttachLargeText =
        !hasFileItem && !!text && text.length >= LONG_TEXT_THRESHOLD;

      // Prevent default synchronously, before any async work, so the
      // filename/text is never inserted into the composer on browsers
      // that process the paste event synchronously.
      if (hasFileItem || shouldAttachLargeText) {
        event.preventDefault();
      }

      const fileItems = items
        .filter((item) => item.kind === 'file')
        .map((item) => item.getAsFile())
        .filter((file): file is File => file != null);

      const newAttachments = await Promise.all(
        fileItems.map(buildAttachmentFromPastedFile),
      );

      if (shouldAttachLargeText) {
        newAttachments.push(buildAttachmentFromPastedText(text));
      }

      if (newAttachments.length > 0) {
        setAttachments((prev) => [...prev, ...newAttachments]);
      }
    },
    [setAttachments],
  );

  const takeScreenshot = useCallback(async (): Promise<void> => {
    let stream: MediaStream | null = null;
    try {
      stream = await navigator.mediaDevices.getDisplayMedia({
        video: { displaySurface: 'browser' },
        audio: false,
        preferCurrentTab: true,
      } as DisplayMediaStreamOptions);
      const video = document.createElement('video');
      video.srcObject = stream;
      await new Promise<void>((resolve) => {
        video.onloadedmetadata = () => {
          void video.play().then(() => resolve());
        };
      });
      const canvas = document.createElement('canvas');
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      canvas.getContext('2d')?.drawImage(video, 0, 0);
      setAttachments((prev) => [
        ...prev,
        {
          id: crypto.randomUUID(),
          name: `screenshot-${Date.now()}.png`,
          type: 'image',
          url: canvas.toDataURL('image/png'),
        },
      ]);
    } catch (error) {
      setError(
        `Screenshot failed: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    } finally {
      stream?.getTracks().forEach((track) => track.stop());
    }
  }, [setAttachments, setError]);

  return { removeAttachment, onFileSelect, onPaste, takeScreenshot };
}
