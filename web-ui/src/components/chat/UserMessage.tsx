import { memo } from 'react';
import type { ApiAttachment } from '@/lib/types';
import { Paperclip } from 'lucide-react';

interface UserMessageProps {
  content: string;
  attachments?: ApiAttachment[];
}

export const UserMessage = memo(function UserMessage({ content, attachments }: UserMessageProps) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[85%] rounded-2xl rounded-tr-md bg-primary/10 px-4 py-2.5 text-sm whitespace-pre-wrap break-words">
        {content}
        {attachments && attachments.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-1.5">
            {attachments.map((a) => (
              <span
                key={a.id}
                className="inline-flex items-center gap-1 rounded-md bg-background/60 border border-border px-2 py-0.5 text-xs"
              >
                <Paperclip className="size-3" /> {a.name}
              </span>
            ))}
          </div>
        )}
      </div>
    </div>
  );
});
