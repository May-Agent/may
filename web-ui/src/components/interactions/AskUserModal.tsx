import { useMemo, useState } from 'react';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import type { PersistedAskUserRequest } from '@/lib/types';
import { respondAskUser } from '@/lib/api/interactions';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { buildAnswers, emptySectionAnswer, type SectionAnswer } from '@/lib/askUserAnswers';

/** The answer form of one `ask_user` request; shared by the blocking modal and the open-question cards. */
export function AskUserForm({ request }: { request: PersistedAskUserRequest }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  const initialState = useMemo(() => {
    const m = new Map<string, SectionAnswer>();
    for (const q of request.questions) {
      m.set(q.section, emptySectionAnswer());
    }
    return m;
  }, [request]);
  const [sections, setSections] = useState(initialState);

  const updateSection = (section: string, mutate: (s: SectionAnswer) => SectionAnswer) => {
    setSections((prev) => {
      const next = new Map(prev);
      const current = next.get(section) ?? emptySectionAnswer();
      next.set(section, mutate(current));
      return next;
    });
  };

  const toggle = (section: string, option: string, multi: boolean) => {
    updateSection(section, (s) => {
      const selection = new Set(s.selection);
      if (multi) {
        if (selection.has(option)) selection.delete(option);
        else selection.add(option);
      } else {
        selection.clear();
        selection.add(option);
      }
      return { ...s, selection, custom: '' };
    });
  };

  const answers = buildAnswers(request, sections);

  const submit = async () => {
    if (!answers) return;
    setBusy(true);
    setFailed(false);
    try {
      await respondAskUser(request.id, answers);
    } catch {
      // The question was withdrawn or answered elsewhere; the next runtime update removes the card.
      setFailed(true);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <div className="space-y-5 py-3 max-h-[60vh] overflow-y-auto scrollbar-thin">
        {request.questions.map((q) => {
          const state = sections.get(q.section) ?? emptySectionAnswer();
          return (
            <div key={q.section} className="space-y-2">
              <div className="text-xs uppercase tracking-wider text-muted-foreground">
                {q.section}
              </div>
              <div className="text-sm font-medium">{q.question}</div>
              <div className="flex flex-wrap gap-1.5">
                {q.option.map((opt) => {
                  const selected = state.selection.has(opt);
                  return (
                    <button
                      key={opt}
                      type="button"
                      onClick={() => toggle(q.section, opt, q.multiSelect)}
                      className={cn(
                        'rounded-md border px-2.5 py-1 text-xs transition-colors',
                        selected
                          ? 'border-foreground/60 bg-accent text-foreground'
                          : 'border-border hover:bg-accent',
                      )}
                    >
                      {opt}
                    </button>
                  );
                })}
              </div>
              <input
                placeholder={t('modals.askUser.other')}
                value={state.custom}
                onChange={(e) => updateSection(q.section, (s) => ({ ...s, custom: e.target.value, selection: new Set() }))}
                className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1 text-sm outline-none focus:ring-2 focus:ring-ring"
              />
            </div>
          );
        })}
      </div>
      <div className="flex items-center justify-end gap-2">
        {failed && <span className="text-xs text-destructive">{t('modals.askUser.failed')}</span>}
        <Button disabled={busy || !answers} onClick={submit}>
          {t('common.submit')}
        </Button>
      </div>
    </>
  );
}

/** A prompt a run is blocked on. */
export function AskUserModal({ request }: { request: PersistedAskUserRequest | null }) {
  const { t } = useTranslation();
  if (!request) return null;
  return (
    <Dialog open>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t('modals.askUser.title')}</DialogTitle>
        </DialogHeader>
        <AskUserForm key={request.id} request={request} />
      </DialogContent>
    </Dialog>
  );
}

/** Questions the agent asked that are neither answered nor withdrawn; no run waits on them. */
export function OpenQuestions({ requests }: { requests: PersistedAskUserRequest[] }) {
  const { t } = useTranslation();
  if (requests.length === 0) return null;
  return (
    <aside
      aria-label={t('modals.askUser.openTitle')}
      className="fixed bottom-4 right-4 z-40 flex max-h-[80vh] w-[min(28rem,calc(100vw-2rem))] flex-col gap-3 overflow-y-auto"
    >
      {requests.map((request) => (
        <section
          key={request.id}
          data-request-id={request.id}
          className="rounded-lg border border-border bg-background p-4 shadow-lg"
        >
          <h2 className="text-sm font-semibold">{t('modals.askUser.openTitle')}</h2>
          <AskUserForm request={request} />
        </section>
      ))}
    </aside>
  );
}
