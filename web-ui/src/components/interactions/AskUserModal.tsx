import { useMemo, useState } from 'react';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import type { PersistedAskUserRequest } from '@/lib/types';
import { respondAskUser, type AskUserAnswerEntry } from '@/lib/api/interactions';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

interface SectionState {
  selection: Set<string>;
  custom: string;
  skip: boolean;
}

export function AskUserModal({ request }: { request: PersistedAskUserRequest | null }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const initialState = useMemo(() => {
    if (!request) return new Map<string, SectionState>();
    const m = new Map<string, SectionState>();
    for (const q of request.questions) {
      m.set(q.section, { selection: new Set(), custom: '', skip: false });
    }
    return m;
  }, [request]);
  const [sections, setSections] = useState(initialState);

  if (!request) return null;

  const updateSection = (section: string, mutate: (s: SectionState) => SectionState) => {
    setSections((prev) => {
      const next = new Map(prev);
      const current = next.get(section) ?? { selection: new Set(), custom: '', skip: false };
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
      return { ...s, selection, skip: false };
    });
  };

  const submit = async () => {
    setBusy(true);
    try {
      const answers: AskUserAnswerEntry[] = request.questions.map((q) => {
        const s = sections.get(q.section) ?? { selection: new Set<string>(), custom: '', skip: false };
        if (s.skip) return { section: q.section, answer: { kind: 'skipped' } };
        if (s.custom.trim().length > 0) {
          return { section: q.section, answer: { kind: 'custom', text: s.custom.trim() } };
        }
        return { section: q.section, answer: { kind: 'selected', values: Array.from(s.selection) } };
      });
      await respondAskUser(request.id, answers);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t('modals.askUser.title')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5 py-3 max-h-[60vh] overflow-y-auto scrollbar-thin">
          {request.questions.map((q) => {
            const state = sections.get(q.section) ?? { selection: new Set<string>(), custom: '', skip: false };
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
                            ? 'border-primary bg-primary/10 text-foreground'
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
                  onChange={(e) => updateSection(q.section, (s) => ({ ...s, custom: e.target.value, skip: false }))}
                  className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1 text-sm outline-none focus:ring-2 focus:ring-ring"
                />
                <label className="flex items-center gap-2 text-xs text-muted-foreground">
                  <input
                    type="checkbox"
                    checked={state.skip}
                    onChange={(e) => updateSection(q.section, (s) => ({ ...s, skip: e.target.checked }))}
                  />
                  {t('modals.askUser.skip')}
                </label>
              </div>
            );
          })}
        </div>
        <div className="flex justify-end gap-2">
          <Button disabled={busy} onClick={submit}>
            {t('common.submit')}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
