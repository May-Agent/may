import { useEffect, useState } from 'react';
import { Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import ReactMarkdown from 'react-markdown';
import { fetchSessionTasks, saveSessionTasks } from '@/lib/api';
import type { SessionPlanState } from '@/types';
import { useStore } from '@/lib/store';

interface TasksPanelProps {
  sessionId: string;
  markdownComponents: any;
}

export function TasksPanel({ sessionId, markdownComponents }: TasksPanelProps) {
  const [content, setContent] = useState('');
  const [draft, setDraft] = useState('');
  const [planState, setPlanState] = useState<SessionPlanState>('disabled');
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const setSessionPlanState = useStore((state) => state.setSessionPlanState);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void fetchSessionTasks(sessionId)
      .then((payload) => {
        if (cancelled) return;
        setContent(payload.content);
        setDraft(payload.content);
        setPlanState(payload.planState);
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

  const persist = async (
    nextContent: string,
    nextPlanState: SessionPlanState,
    nextEditing: boolean,
  ) => {
    setSaving(true);
    setError(null);
    try {
      const payload = await saveSessionTasks(sessionId, {
        content: nextContent,
        planState: nextPlanState,
      });
      setContent(payload.content);
      setDraft(payload.content);
      setPlanState(payload.planState);
      setEditing(nextEditing);
      setSessionPlanState(sessionId, payload.planState);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="not-prose mt-3 rounded-xl border border-border/70 bg-card/80 px-3 py-3">
      <div className="mb-2 flex items-center justify-between gap-3">
        <div>
          <div className="text-sm font-medium text-foreground">tasks.md</div>
          <div className="text-xs text-muted-foreground">
            Session task plan exposed via `&lt;tasks /&gt;`
          </div>
          <div className="mt-1 text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
            plan mode: {planState}
          </div>
        </div>
        <div className="flex items-center gap-2">
          {planState === 'disabled' && (
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-7 px-2 text-xs"
              disabled={loading || saving}
              onClick={() => void persist(draft, 'draft', editing)}
            >
              Enter Plan
            </Button>
          )}
          {planState === 'draft' && (
            <Button
              type="button"
              size="sm"
              className="h-7 px-2 text-xs"
              disabled={loading || saving}
              onClick={() => void persist(draft, 'approved', editing)}
            >
              Approve
            </Button>
          )}
          {planState === 'approved' && (
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-7 px-2 text-xs"
              disabled={loading || saving}
              onClick={() => void persist(draft, 'draft', editing)}
            >
              Reopen Plan
            </Button>
          )}
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="h-7 px-2 text-xs"
            disabled={loading || saving}
            onClick={() => {
              setEditing((value) => !value);
              setDraft(content);
            }}
          >
            {editing ? 'Preview' : 'Edit'}
          </Button>
          {editing && (
            <Button
              type="button"
              size="sm"
              className="h-7 px-2 text-xs"
              disabled={saving}
              onClick={() => void persist(draft, planState, false)}
            >
              {saving ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : null}
              Save
            </Button>
          )}
        </div>
      </div>
      {error && (
        <div className="mb-2 rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-2 text-xs text-destructive">
          {error}
        </div>
      )}
      {loading ? (
        <div className="text-xs text-muted-foreground">Loading tasks.md…</div>
      ) : editing ? (
        <textarea
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          className="min-h-[220px] w-full rounded-lg border border-border bg-background px-3 py-2 font-mono text-xs text-foreground outline-none focus:ring-2 focus:ring-ring"
        />
      ) : content.trim() ? (
        <div className="rounded-lg border border-border/60 bg-background/70 px-3 py-2">
          <ReactMarkdown components={markdownComponents}>{content}</ReactMarkdown>
        </div>
      ) : (
        <div className="text-xs text-muted-foreground">tasks.md is empty.</div>
      )}
    </div>
  );
}
