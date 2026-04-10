import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import type {
  AskUserAnswer,
  AskUserRequest,
  PermissionRequest,
  WorkspaceRequest,
} from '@/lib/api';

// ── Permission ─────────────────────────────────────────────

export function PermissionPrompt({
  request,
  isSubmitting,
  onDecide,
}: {
  request: PermissionRequest;
  isSubmitting: boolean;
  onDecide: (approved: boolean) => void;
}) {
  const tone =
    request.mode === 'unsafe'
      ? 'border-red-300 bg-red-100 text-red-900 dark:border-red-800 dark:bg-red-950/95 dark:text-red-100'
      : 'border-border bg-card text-foreground';
  return (
    <div className={`mb-3 rounded-lg border px-3 py-2 text-sm ${tone}`}>
      <div className="font-medium">Approval required ({request.mode})</div>
      <div className="mt-1 break-all">{request.script}</div>
      <div className="mt-2 flex gap-2">
        <Button
          size="sm"
          variant="secondary"
          disabled={isSubmitting}
          onClick={() => onDecide(true)}
        >
          Approve
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={isSubmitting}
          onClick={() => onDecide(false)}
        >
          Deny
        </Button>
      </div>
    </div>
  );
}

// ── Workspace ──────────────────────────────────────────────

export function WorkspacePrompt({
  request,
  isSubmitting,
  onDecide,
}: {
  request: WorkspaceRequest;
  isSubmitting: boolean;
  onDecide: (approved: boolean) => void;
}) {
  return (
    <div className="mb-3 rounded-lg border border-border bg-card px-3 py-2 text-sm text-foreground">
      <div className="font-medium">Workspace access required</div>
      <div className="mt-1 break-all font-mono text-xs">{request.path}</div>
      <div className="mt-2 text-muted-foreground">{request.reason}</div>
      <div className="mt-2 flex gap-2">
        <Button
          size="sm"
          variant="secondary"
          disabled={isSubmitting}
          onClick={() => onDecide(true)}
        >
          Approve
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={isSubmitting}
          onClick={() => onDecide(false)}
        >
          Deny
        </Button>
      </div>
    </div>
  );
}

// ── Ask user ───────────────────────────────────────────────

export interface AskUserState {
  request: AskUserRequest;
  answers: AskUserAnswer[];
}

export function AskUserPrompt({
  state,
  isSubmitting,
  onAnswerChange,
  onSubmit,
  onSkipAll,
}: {
  state: AskUserState;
  isSubmitting: boolean;
  onAnswerChange: (
    questionIndex: number,
    value: AskUserAnswer['answer'],
  ) => void;
  onSubmit: () => void;
  onSkipAll: () => void;
}) {
  return (
    <div className="mb-3 rounded-xl border-2 border-primary/40 bg-card px-4 py-3 shadow-xl">
      <div className="flex items-center justify-between gap-3">
        <div className="text-sm font-semibold text-foreground">
          Input required
        </div>
        <div className="text-xs text-muted-foreground">
          {state.request.questions.length} question
          {state.request.questions.length > 1 ? 's' : ''}
        </div>
      </div>

      <div className="mt-3 max-h-[36vh] overflow-y-auto space-y-3 pr-1">
        {state.request.questions.map((question, idx) => {
          const answer = state.answers[idx]?.answer;
          const selected = answer?.kind === 'selected' ? answer.values : [];
          const custom = answer?.kind === 'custom' ? answer.text : '';

          return (
            <div
              key={`${question.section}-${idx}`}
              className="rounded-lg border border-border bg-background p-3"
            >
              <div className="flex items-start justify-between gap-2">
                <div className="text-sm font-medium text-foreground leading-snug">
                  {question.question}
                </div>
                {question.section ? (
                  <span className="text-[10px] uppercase tracking-wide text-muted-foreground border border-border rounded-full px-2 py-0.5">
                    {question.section}
                  </span>
                ) : null}
              </div>

              {question.option.length > 0 && (
                <div className="mt-2 flex flex-col gap-2">
                  {question.option.map((option) => {
                    const active = selected.includes(option);
                    return (
                      <Button
                        key={option}
                        size="sm"
                        variant={active ? 'default' : 'outline'}
                        className="h-8 w-full justify-start"
                        disabled={isSubmitting}
                        onClick={() => {
                          if (question.multiSelect) {
                            const next = active
                              ? selected.filter((v) => v !== option)
                              : [...selected, option];
                            onAnswerChange(
                              idx,
                              next.length > 0
                                ? { kind: 'selected', values: next }
                                : { kind: 'skipped' },
                            );
                          } else {
                            onAnswerChange(idx, {
                              kind: 'selected',
                              values: [option],
                            });
                          }
                        }}
                      >
                        {option}
                      </Button>
                    );
                  })}
                </div>
              )}

              <Input
                value={custom}
                onChange={(e) =>
                  onAnswerChange(
                    idx,
                    e.target.value.trim().length > 0
                      ? { kind: 'custom', text: e.target.value }
                      : { kind: 'skipped' },
                  )
                }
                placeholder="Custom answer (optional)"
                className="mt-2 h-9 bg-background"
                disabled={isSubmitting}
              />
            </div>
          );
        })}
      </div>

      <div className="mt-3 flex items-center justify-end gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={isSubmitting}
          onClick={onSkipAll}
        >
          Skip all
        </Button>
        <Button size="sm" disabled={isSubmitting} onClick={onSubmit}>
          Submit
        </Button>
      </div>
    </div>
  );
}
