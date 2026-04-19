import { useEffect, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import { fetchSessionTasks } from '@/lib/api';

interface TasksPanelProps {
  sessionId: string;
  markdownComponents: any;
}

/** Read-only renderer for the agent-facing `tasks.md` file. The model
 *  drives plan state through inline `<may-plan>` blocks now; this
 *  component only surfaces the persisted file when the agent emits the
 *  `<tasks />` placeholder. */
export function TasksPanel({ sessionId, markdownComponents }: TasksPanelProps) {
  const [content, setContent] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void fetchSessionTasks(sessionId)
      .then((payload) => {
        if (cancelled) return;
        setContent(payload.content);
      })
      .catch((err) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  const trimmed = content.trim();

  return (
    <div className="not-prose mt-3 rounded-xl border border-border/70 bg-card/80 px-3 py-3">
      <div className="mb-2">
        <div className="text-sm font-medium text-foreground">tasks.md</div>
        <div className="text-xs text-muted-foreground">
          Session task file surfaced via <code>&lt;tasks /&gt;</code>
        </div>
      </div>
      {error && (
        <div className="mb-2 rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-2 text-xs text-destructive">
          {error}
        </div>
      )}
      {loading ? (
        <div className="text-xs text-muted-foreground">Loading tasks.md…</div>
      ) : trimmed ? (
        <div className="rounded-lg border border-border/60 bg-background/70 px-3 py-2">
          <ReactMarkdown components={markdownComponents}>{content}</ReactMarkdown>
        </div>
      ) : (
        <div className="text-xs text-muted-foreground">tasks.md is empty.</div>
      )}
    </div>
  );
}
