import { Dialog, DialogContent, DialogTrigger, DialogTitle } from '@/components/ui/dialog';
import type { Attachment } from '@/types';
import { File } from 'lucide-react';

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
      <DialogContent className="max-w-4xl w-full p-0 overflow-hidden bg-transparent border-none shadow-none sm:rounded-lg">
        <div className="relative w-full h-full flex flex-col items-center justify-center">
          <DialogTitle className="sr-only">
            Preview {attachment.name}
          </DialogTitle>
          {attachment.type === 'image' ? (
            <img 
              src={attachment.url} 
              alt={attachment.name} 
              className="max-h-[85vh] w-auto object-contain rounded-md"
            />
          ) : (
            <div className="bg-background p-8 rounded-lg flex flex-col items-center gap-4">
              <File className="h-16 w-16 text-muted-foreground" />
              <div className="text-center">
                <p className="font-medium text-lg">{attachment.name}</p>
                <p className="text-sm text-muted-foreground">Preview not available for this file type</p>
              </div>
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
