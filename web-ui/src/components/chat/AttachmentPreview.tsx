import { Dialog, DialogContent, DialogTrigger, DialogTitle } from '@/components/ui/dialog';
import type { Attachment } from '@/types';
import { File, Download } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentKindIcon } from './AttachmentCard';

interface AttachmentPreviewProps {
  attachment: Attachment;
  children: React.ReactNode;
}

export function AttachmentPreview({ attachment, children }: AttachmentPreviewProps) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <div className="cursor-pointer transition-opacity hover:opacity-80">
          {children}
        </div>
      </DialogTrigger>
      <DialogContent className="max-w-4xl w-full max-h-[90vh] overflow-hidden bg-background border sm:rounded-lg p-0">
        <DialogTitle className="sr-only">
          Preview: {attachment.name}
        </DialogTitle>
        <div className="flex flex-col h-full">
          <div className="flex items-center justify-between px-4 py-3 border-b">
            <div className="flex items-center gap-2 min-w-0">
              <AttachmentKindIcon attachment={attachment} />
              <span className="text-sm font-medium truncate">{attachment.name}</span>
            </div>
            {attachment.url && (
              <Button variant="ghost" size="sm" asChild className="flex-shrink-0">
                <a href={attachment.url} download={attachment.name}>
                  <Download className="h-4 w-4 mr-1" /> Download
                </a>
              </Button>
            )}
          </div>

          <div className="flex-1 overflow-auto">
            {attachment.type === 'image' ? (
              <div className="flex items-center justify-center p-4">
                <img
                  src={attachment.url}
                  alt={attachment.name}
                  className="max-h-[75vh] w-auto object-contain rounded-md"
                />
              </div>
            ) : attachment.type === 'pdf' ? (
              <iframe
                src={attachment.url}
                className="w-full h-[75vh] border-0"
                title={attachment.name}
              />
            ) : attachment.type === 'document' ? (
              <div className="p-6 max-h-[75vh] overflow-auto">
                <pre className="whitespace-pre-wrap text-sm leading-relaxed font-sans text-foreground">
                  {attachment.textContent || 'No content available'}
                </pre>
              </div>
            ) : (
              <div className="flex flex-col items-center justify-center gap-4 p-12">
                <File className="h-16 w-16 text-muted-foreground" />
                <div className="text-center">
                  <p className="font-medium">{attachment.name}</p>
                  <p className="text-sm text-muted-foreground">Preview not available</p>
                </div>
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
