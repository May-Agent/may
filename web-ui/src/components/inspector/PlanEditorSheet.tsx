import { useState } from 'react';
import { Loader2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { putSessionTasks } from '@/lib/api/tasks';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useInspectorStore } from '@/lib/stores/inspector';
import { useTranslation } from '@/lib/i18n';

interface PlanEditorSheetProps {
  sessionId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  content: string;
}

/**
 * Edit tasks.md in a sheet over the chat. Saving returns the plan to draft,
 * as the plan banner's inline editor does.
 */
export function PlanEditorSheet(props: PlanEditorSheetProps) {
  // Mounted only while open, so the editor starts from the current document.
  return props.open ? <OpenPlanEditorSheet {...props} /> : null;
}

function OpenPlanEditorSheet({ sessionId, open, onOpenChange, content }: PlanEditorSheetProps) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const setTasks = useInspectorStore((s) => s.setTasks);
  const [draft, setDraft] = useState(content);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    setBusy(true);
    try {
      const res = await putSessionTasks(sessionId, { content: draft, planState: 'draft' });
      setTasks(sessionId, res.content, 'draft');
      updateRuntime(sessionId, { planState: 'draft' });
      onOpenChange(false);
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <SheetContent side="right" className="max-w-xl">
        <SheetHeader>
          <SheetTitle>{t('plan.editTitle')}</SheetTitle>
          <SheetDescription>{t('plan.editDescription')}</SheetDescription>
        </SheetHeader>
        <div className="flex min-h-0 flex-1 flex-col p-4">
          <Textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void save();
              }
            }}
            aria-label={t('plan.editTitle')}
            className="min-h-0 flex-1 resize-none font-mono text-[13px] leading-5"
            autoFocus
          />
        </div>
        <div className="flex justify-end gap-2 border-t border-border px-4 py-3">
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button onClick={() => void save()} disabled={busy || draft === content} aria-busy={busy}>
            {busy && <Loader2 className="animate-spin" aria-hidden />}
            {t('common.save')}
          </Button>
        </div>
      </SheetContent>
    </Sheet>
  );
}
