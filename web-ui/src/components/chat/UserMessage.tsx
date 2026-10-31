import { memo } from 'react';
import type { ApiAttachment } from '@/lib/types';
import { Paperclip } from 'lucide-react';
import { MessageActions } from './MessageActions';

interface UserMessageProps {
  content: string;
  attachments?: ApiAttachment[];
}

/** Your message: an ink bubble on the right, actions beside it on hover. */
export const UserMessage = memo(function UserMessage({ content, attachments }: UserMessageProps) {
  return (
    <div className="group/message flex items-end justify-end gap-1.5">
      <MessageActions text={content} className="mb-1" />
      <div className="max-w-[80%] rounded-2xl bg-neutral-900 px-3.5 py-2 text-sm text-white whitespace-pre-wrap break-words dark:bg-neutral-800 dark:text-neutral-100">
        {content}
        {attachments && attachments.length > 0 && (
          <div className="mt-2 flex flex-wrap gap-1.5">
            {attachments.map((a) => (
              <span
                key={a.id}
                className="inline-flex items-center gap-1 rounded-md bg-white/10 px-2 py-0.5 text-xs"
              >
                <Paperclip className="size-3" aria-hidden /> {a.name}
              </span>
            ))}
          </div>
        )}
      </div>
    </div>
  );
});
