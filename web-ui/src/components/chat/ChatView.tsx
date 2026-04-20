import { useEffect, useRef } from 'react';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTurnStore } from '@/lib/stores/turn';
import { ScrollArea } from '@/components/ui/scroll-area';
import { AssistantTurn } from './AssistantTurn';
import { UserMessage } from './UserMessage';
import { PersistedAssistant } from './PersistedAssistant';
import { Composer } from './Composer';

export function ChatView({ sessionId }: { sessionId: string }) {
  const open = useSessionsStore((s) => s.open);
  const messages = useSessionsStore((s) => s.activeMessages);
  const active = useSessionsStore((s) => s.active);
  const appendMessage = useSessionsStore((s) => s.appendMessage);
  const activeTurn = useTurnStore((s) => s.active);
  const send = useTurnStore((s) => s.send);
  const cancel = useTurnStore((s) => s.cancel);

  const scrollerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    open(sessionId);
  }, [sessionId, open]);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [messages.length, activeTurn?.text.length, activeTurn?.tools.length, activeTurn?.reasoning.length]);

  if (!active || active.id !== sessionId) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        Loading session…
      </div>
    );
  }

  const streaming = activeTurn !== null;

  const handleSend = async (content: string) => {
    const userMessage = {
      id: `local-${Date.now()}`,
      role: 'user' as const,
      content,
      timestamp: new Date().toISOString(),
    };
    appendMessage(userMessage);
    await send({ sessionId, content });
  };

  return (
    <div className="flex h-full flex-col">
      <ScrollArea ref={scrollerRef} className="flex-1 px-6 py-6">
        <div className="mx-auto w-full max-w-3xl space-y-6">
          {messages.map((msg) =>
            msg.role === 'user' ? (
              <UserMessage key={msg.id} content={msg.content} attachments={msg.attachments} />
            ) : (
              <PersistedAssistant key={msg.id} message={msg} />
            ),
          )}
          {activeTurn && <AssistantTurn turn={activeTurn} />}
        </div>
      </ScrollArea>
      <div className="border-t border-border bg-background/70 px-6 py-4 backdrop-blur">
        <div className="mx-auto w-full max-w-3xl">
          <Composer streaming={streaming} onSend={handleSend} onCancel={cancel} />
        </div>
      </div>
    </div>
  );
}
