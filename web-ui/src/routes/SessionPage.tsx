import { useParams } from 'react-router-dom';
import { ChatView } from '@/components/chat/ChatView';

export function SessionPage() {
  const { sessionId } = useParams();
  if (!sessionId) return null;
  return <ChatView sessionId={sessionId} />;
}
