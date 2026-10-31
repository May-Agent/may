import { useEffect, useState } from 'react';
import { Streamdown } from 'streamdown';
import {
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  ClipboardList,
  Pencil,
  Save,
  Trash2,
  X,
} from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Textarea } from '@/components/ui/textarea';
import {
  getSessionTasks,
  putSessionPlanState,
  putSessionTasks,
} from '@/lib/api/tasks';
import { useTranslation } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useInspectorStore } from '@/lib/stores/inspector';
import { ConfirmDialog } from '@/components/ui/confirm-dialog';
import type { SessionPlanState } from '@/lib/types';
import { cn } from '@/lib/utils';

interface PlanBannerProps {
  sessionId: string;
  planState: SessionPlanState;
}

export function PlanBanner({ sessionId, planState }: PlanBannerProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const [content, setContent] = useState<string>('');
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState(false);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const [busy, setBusy] = useState(false);
  const [discarding, setDiscarding] = useState(false);
  const setInspectorTasks = useInspectorStore((s) => s.setTasks);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await getSessionTasks(sessionId);
        if (!cancelled) setContent(res.content);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [sessionId, planState]);

  // The banner exists for the one decision only the user can make: approving
  // a draft plan. An approved plan lives in the inspector.
  if (planState !== 'draft' && !editing) return null;
  if (loading && !content) return null;

  const approved = planState === 'approved';

  const transition = async (next: SessionPlanState) => {
    setBusy(true);
    try {
      await putSessionPlanState(sessionId, next);
      updateRuntime(sessionId, { planState: next });
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const onSave = async () => {
    setBusy(true);
    try {
      const res = await putSessionTasks(sessionId, { content: draft, planState: 'draft' });
      setContent(res.content);
      setInspectorTasks(sessionId, res.content, 'draft');
      updateRuntime(sessionId, { planState: 'draft' });
      setEditing(false);
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const beginEdit = () => {
    setDraft(content);
    setEditing(true);
    setExpanded(true);
  };

  return (
    <div
      className={cn(
        'border-b border-border bg-gradient-to-b from-card/40 to-transparent',
      )}
    >
      <ConfirmDialog
        open={discarding}
        onOpenChange={setDiscarding}
        title={t('plan.discardTitle')}
        description={t('plan.discardBody')}
        confirmLabel={t('chat.discardPlan')}
        destructive
        onConfirm={() => transition('disabled')}
      />
      <div className="mx-auto w-full max-w-3xl px-4 py-3 sm:px-6">
        <div className="flex items-start gap-2">
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="-m-1.5 flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground touch:size-11"
            aria-label={expanded ? t('plan.collapse') : t('plan.expand')}
            aria-expanded={expanded}
          >
            {expanded ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
          </button>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <ClipboardList className="size-3.5 text-muted-foreground" />
              <span className="text-xs font-medium text-foreground">
                {t('chat.plan')}
              </span>
              <Badge tone={approved ? 'positive' : 'info'}>
                {approved ? (
                  <>
                    <CheckCircle2 className="size-2.5" />
                    {t('chat.planStateApproved')}
                  </>
                ) : (
                  t('chat.planStateDraft')
                )}
              </Badge>
            </div>

            {!expanded && content.length > 0 && (
              <div className="mt-1 truncate text-[11px] text-muted-foreground">
                {firstHeading(content) ?? content.slice(0, 120)}
              </div>
            )}

            {expanded && !editing && (
              <div className="mt-2 max-h-64 overflow-y-auto rounded-md border border-border/60 bg-background/40 px-3 py-2 text-sm">
                {content.trim().length === 0 ? (
                  <div className="text-xs text-muted-foreground">{t('plan.noContent')}</div>
                ) : (
                  <div className="prose prose-sm dark:prose-invert max-w-none prose-headings:mb-1 prose-p:my-1 prose-ul:my-1">
                    <Streamdown>{content}</Streamdown>
                  </div>
                )}
              </div>
            )}

            {expanded && editing && (
              <Textarea
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                className="mt-2 min-h-[160px] font-mono text-xs"
                autoFocus
              />
            )}
          </div>

          <div className="flex shrink-0 items-center gap-1">
            {!editing ? (
              <>
                {!approved && (
                  <Button size="sm" onClick={() => transition('approved')} disabled={busy}>
                    <CheckCircle2 className="size-3.5" />
                    {t('chat.approvePlan')}
                  </Button>
                )}
                <Button
                  size="icon"
                  variant="ghost"
                  onClick={beginEdit}
                  aria-label={t('plan.edit')}
                  title={t('plan.edit')}
                >
                  <Pencil />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="text-destructive hover:bg-destructive/10 hover:text-destructive"
                  onClick={() => setDiscarding(true)}
                  disabled={busy}
                  aria-label={t('chat.discardPlan')}
                  title={t('chat.discardPlan')}
                >
                  <Trash2 />
                </Button>
              </>
            ) : (
              <>
                <Button size="sm" onClick={onSave} disabled={busy}>
                  <Save className="size-3.5" />
                  {t('common.save')}
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  onClick={() => setEditing(false)}
                  disabled={busy}
                  aria-label={t('common.cancel')}
                >
                  <X className="size-3.5" />
                </Button>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function firstHeading(markdown: string): string | null {
  for (const line of markdown.split('\n')) {
    const trimmed = line.trim();
    if (trimmed.startsWith('#')) return trimmed.replace(/^#+\s*/, '');
    if (trimmed.length > 0) return trimmed;
  }
  return null;
}
