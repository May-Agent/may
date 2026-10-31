import { useEffect, useState } from 'react';
import { Streamdown } from 'streamdown';
import { ChevronRight, Pencil, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
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

  // The plan's state lives here, with the plan, and nowhere else: a draft
  // waits for the user's approval, an approved plan says so quietly.
  if (planState === 'disabled' && !editing) return null;
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
    <div className="border-b border-border">
      <ConfirmDialog
        open={discarding}
        onOpenChange={setDiscarding}
        title={t('plan.discardTitle')}
        description={t('plan.discardBody')}
        confirmLabel={t('chat.discardPlan')}
        destructive
        onConfirm={() => transition('disabled')}
      />
      <div className="mx-auto w-full max-w-3xl px-4 py-2 sm:px-6">
        <div className="flex items-start gap-2">
          <div className="min-w-0 flex-1">
            <button
              type="button"
              onClick={() => setExpanded((v) => !v)}
              className="-mx-1.5 flex h-7 max-w-full items-center gap-1.5 rounded-md px-1.5 text-[13px] hover:bg-accent touch:h-11"
              aria-expanded={expanded}
              title={expanded ? t('plan.collapse') : t('plan.expand')}
            >
              <ChevronRight
                className={cn('size-3.5 shrink-0 text-muted-foreground transition-transform', expanded && 'rotate-90')}
                aria-hidden
              />
              <span className="shrink-0 font-medium">{t('chat.plan')}</span>
              <span className="shrink-0 text-muted-foreground">
                {approved ? t('plan.approved') : t('plan.awaiting')}
              </span>
              {!expanded && content.length > 0 && (
                <span className="min-w-0 truncate text-muted-foreground/80">
                  · {firstHeading(content) ?? content.slice(0, 120)}
                </span>
              )}
            </button>

            {expanded && !editing && (
              <div className="mt-1 mb-1 max-h-64 overflow-y-auto border-l border-border pl-3 text-[13px]">
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
            {approved && !editing ? null : !editing ? (
              <>
                <Button size="sm" onClick={() => transition('approved')} disabled={busy}>
                  {t('chat.approvePlan')}
                </Button>
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
                  className="hover:text-destructive"
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
                <Button size="sm" variant="ghost" onClick={() => setEditing(false)} disabled={busy}>
                  {t('common.cancel')}
                </Button>
                <Button size="sm" onClick={onSave} disabled={busy}>
                  {t('common.save')}
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
