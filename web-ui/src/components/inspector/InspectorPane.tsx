import { useEffect, useMemo, useState, type ReactNode } from 'react';
import * as DropdownMenu from '@radix-ui/react-dropdown-menu';
import {
  Bot,
  Loader2,
  MoreHorizontal,
  PanelRightClose,
  Pencil,
  Square,
  SquareTerminal,
  Trash2,
  Undo2,
  X,
} from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { ScrollArea } from '@/components/ui/scroll-area';
import { ConfirmDialog } from '@/components/ui/confirm-dialog';
import { RenameDialog } from '@/components/ui/rename-dialog';
import { RuntimePhaseBadge } from '@/components/shell/RuntimePhaseDot';
import { useTranslation } from '@/lib/i18n';
import { EMPTY_INSPECTOR_ENTRY, useInspectorStore } from '@/lib/stores/inspector';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTurnStore } from '@/lib/stores/turn';
import { useUiStore } from '@/lib/stores/ui';
import { putSessionPlanState } from '@/lib/api/tasks';
import {
  formatElapsed,
  formatTokens,
  isJobRunning,
  orderJobs,
  scriptHeadline,
} from '@/lib/inspector/model';
import type { BackgroundJob, SubagentRun } from '@/lib/api/background';
import type { ApiSession, SessionPlanState } from '@/lib/types';
import { shortcutAria, shortcutLabel } from '@/lib/shortcuts';
import { cn } from '@/lib/utils';
import { IssueChip, TaskTreeView } from './TaskTreeView';
import { PlanEditorSheet } from './PlanEditorSheet';

type PendingStop =
  | { kind: 'job'; job: BackgroundJob }
  | { kind: 'subagent'; run: SubagentRun }
  | null;

/**
 * The session inspector: name and state, the tasks.md task tree with its
 * linked issues, running subagents and background terminal jobs. Data comes
 * from the inspector store, which the session's event stream keeps current.
 */
export function InspectorPane({ session, inSheet = false }: { session: ApiSession; inSheet?: boolean }) {
  const { t } = useTranslation();
  const sessionId = session.id;
  const entry = useInspectorStore((s) => s.sessions[sessionId] ?? EMPTY_INSPECTOR_ENTRY);
  const loadTasks = useInspectorStore((s) => s.loadTasks);
  const loadBackground = useInspectorStore((s) => s.loadBackground);
  const killJob = useInspectorStore((s) => s.killJob);
  const stopSubagent = useInspectorStore((s) => s.stopSubagent);
  const rename = useSessionsStore((s) => s.rename);
  const toggleInspector = useUiStore((s) => s.toggleInspector);
  const setInspectorSheetOpen = useUiStore((s) => s.setInspectorSheetOpen);
  const todo = useTurnStore((s) => s.turns[sessionId]?.active?.todo ?? null);
  const turnActive = useTurnStore((s) => Boolean(s.turns[sessionId]?.active));
  const planState: SessionPlanState = session.runtime?.planState ?? 'disabled';

  const [renaming, setRenaming] = useState(false);
  const [pendingStop, setPendingStop] = useState<PendingStop>(null);

  useEffect(() => {
    void loadBackground(sessionId);
  }, [sessionId, loadBackground]);

  // tasks.md changes reach the client as `todo_update` turn events; reload
  // the document then, when a turn ends and when the plan state moves.
  useEffect(() => {
    void loadTasks(sessionId);
  }, [sessionId, loadTasks, todo, turnActive, planState]);

  const tasks = entry.tasks;
  const jobs = useMemo(() => orderJobs(entry.background?.jobs ?? []), [entry.background]);
  const subagents = entry.background?.subagents ?? [];
  const runningJobs = jobs.filter(isJobRunning).length;

  const close = () => (inSheet ? setInspectorSheetOpen(false) : toggleInspector());

  const confirmStop = async () => {
    if (!pendingStop) return;
    try {
      if (pendingStop.kind === 'job') await killJob(sessionId, pendingStop.job.pid);
      else await stopSubagent(sessionId, pendingStop.run.runId);
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : String(err),
      });
      throw err;
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col bg-sidebar">
      <div className="flex h-12 shrink-0 items-center gap-2 border-b border-border pl-4 pr-2 touch:h-14">
        <h2 className="flex-1 text-[13px] font-semibold">{t('inspector.title')}</h2>
        <Button
          size="icon"
          variant="ghost"
          onClick={close}
          aria-label={inSheet ? t('common.close') : t('shell.hideInspector')}
          aria-keyshortcuts={inSheet ? undefined : shortcutAria('toggleInspector')}
          title={inSheet ? t('common.close') : `${t('shell.hideInspector')} (${shortcutLabel('toggleInspector')})`}
        >
          {inSheet ? <X /> : <PanelRightClose />}
        </Button>
      </div>

      <ScrollArea className="min-h-0 flex-1">
        <div className="divide-y divide-border">
          <Section title={t('inspector.session')}>
            <div className="flex items-start gap-2">
              <p className="min-w-0 flex-1 break-words text-[15px] font-semibold leading-5">
                {session.title || t('common.untitled')}
              </p>
              <Button
                size="icon"
                variant="ghost"
                className="-mr-1 -mt-1.5 shrink-0"
                onClick={() => setRenaming(true)}
                aria-label={t('inspector.renameSession')}
                title={t('inspector.renameSession')}
              >
                <Pencil />
              </Button>
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              <RuntimePhaseBadge state={session.runtime?.state} />
              {planState !== 'disabled' && (
                <Badge tone={planState === 'approved' ? 'positive' : 'info'}>
                  {planState === 'approved' ? t('chat.planStateApproved') : t('chat.planStateDraft')}
                </Badge>
              )}
              <span className="truncate text-xs text-muted-foreground">{session.model}</span>
            </div>
          </Section>

          <Section
            title={t('inspector.tasks')}
            busy={entry.tasksLoading && tasks !== null}
            accessory={
              tasks && (tasks.content.trim().length > 0 || planState !== 'disabled') ? (
                <PlanMenu
                  sessionId={sessionId}
                  planState={planState}
                  content={tasks.content}
                />
              ) : null
            }
          >
            {tasks === null ? (
              entry.tasksError ? (
                <ErrorLine message={entry.tasksError} />
              ) : (
                <Waiting label={t('inspector.tasksLoading')} />
              )
            ) : (
              <>
                {tasks.tree.roots.length > 0 && (
                  <div className="mb-3">
                    <h4 className="mb-1 text-xs font-semibold text-muted-foreground">
                      {t('inspector.linkedIssues')}
                    </h4>
                    <div className="flex flex-wrap gap-1">
                      {tasks.tree.roots.map((issue) => (
                        <IssueChip key={issue.label} issue={issue} />
                      ))}
                    </div>
                  </div>
                )}
                {tasks.tree.total === 0 ? (
                  <Empty>{t('inspector.tasksEmpty')}</Empty>
                ) : (
                  <>
                    <Progress completed={tasks.tree.completed} total={tasks.tree.total} />
                    <TaskTreeView tree={tasks.tree} />
                  </>
                )}
              </>
            )}
          </Section>

          <Section title={t('inspector.subagents')} count={subagents.length}>
            {entry.backgroundError && !entry.background ? (
              <ErrorLine message={entry.backgroundError} />
            ) : subagents.length === 0 ? (
              <Empty>{t('inspector.subagentsEmpty')}</Empty>
            ) : (
              <ul className="space-y-2">
                {subagents.map((run) => {
                  const stopping = entry.stopping.subagents.includes(run.runId);
                  return (
                    <Row
                      key={run.runId}
                      icon={<Bot className="size-4 text-primary" aria-hidden />}
                      title={run.agent}
                      detail={run.prompt}
                      meta={`${formatElapsed(run.elapsedMs)} · ${t('inspector.subagentTokens', {
                        tokens: formatTokens(run.promptTokens + run.completionTokens),
                        requests: run.requests,
                      })}`}
                      action={
                        <StopButton
                          label={t('inspector.stopSubagent', { agent: run.agent })}
                          stopping={stopping}
                          onClick={() => setPendingStop({ kind: 'subagent', run })}
                        />
                      }
                    />
                  );
                })}
              </ul>
            )}
          </Section>

          <Section title={t('inspector.jobs')} count={runningJobs}>
            {entry.backgroundError && !entry.background ? (
              <ErrorLine message={entry.backgroundError} />
            ) : jobs.length === 0 ? (
              <Empty>{t('inspector.jobsEmpty')}</Empty>
            ) : (
              <ul className="space-y-2">
                {jobs.map((job) => {
                  const running = isJobRunning(job);
                  const stopping = entry.stopping.jobs.includes(job.pid);
                  const headline = scriptHeadline(job.script);
                  return (
                    <Row
                      key={job.pid}
                      icon={<SquareTerminal className="size-4 text-muted-foreground" aria-hidden />}
                      title={<code className="font-mono text-[12px]">{headline}</code>}
                      titleText={job.script}
                      meta={
                        <span className="inline-flex items-center gap-1.5">
                          <JobStatus job={job} stopping={stopping} />
                          <span aria-hidden>·</span>
                          {formatElapsed(job.elapsedMs)}
                        </span>
                      }
                      action={
                        running ? (
                          <StopButton
                            label={t('inspector.killJobNamed', { name: headline })}
                            stopping={stopping}
                            onClick={() => setPendingStop({ kind: 'job', job })}
                          />
                        ) : null
                      }
                    />
                  );
                })}
              </ul>
            )}
          </Section>
        </div>
      </ScrollArea>

      <RenameDialog
        open={renaming}
        onOpenChange={setRenaming}
        title={t('shell.renameTitle')}
        label={t('shell.renameLabel')}
        initialValue={session.title}
        onSubmit={(next) => rename(sessionId, next)}
      />
      <ConfirmDialog
        open={pendingStop !== null}
        onOpenChange={(open) => !open && setPendingStop(null)}
        title={pendingStop?.kind === 'subagent' ? t('inspector.stopSubagent', { agent: pendingStop.run.agent }) : t('inspector.killConfirmTitle')}
        description={pendingStop?.kind === 'job' ? t('inspector.killConfirmBody') : pendingStop?.run.prompt}
        confirmLabel={pendingStop?.kind === 'job' ? t('inspector.killJob') : t('common.stop')}
        destructive
        onConfirm={confirmStop}
      />
    </div>
  );
}

interface SectionProps {
  title: string;
  count?: number;
  busy?: boolean;
  accessory?: ReactNode;
  children: ReactNode;
}

function Section({ title, count, busy, accessory, children }: SectionProps) {
  return (
    <section aria-label={title} className="px-4 py-4">
      <div className="mb-2 flex min-h-6 items-center gap-2">
        <h3 className="text-xs font-semibold text-muted-foreground">{title}</h3>
        {count !== undefined && count > 0 && (
          <span className="rounded-full bg-primary/10 px-1.5 text-[11px] font-semibold tabular-nums text-primary">
            {count}
          </span>
        )}
        {busy && <Loader2 className="size-3.5 animate-spin text-muted-foreground" aria-hidden />}
        <div className="ml-auto">{accessory}</div>
      </div>
      {children}
    </section>
  );
}

function Progress({ completed, total }: { completed: number; total: number }) {
  const { t } = useTranslation();
  const label = t('inspector.tasksProgress', { completed, total });
  return (
    <div className="mb-3">
      <div
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={completed}
        aria-label={label}
        className="h-1.5 overflow-hidden rounded-full bg-accent"
      >
        <div
          className="h-full rounded-full bg-success transition-[width]"
          style={{ width: `${total === 0 ? 0 : (completed / total) * 100}%` }}
        />
      </div>
      <p className="mt-1 text-xs text-muted-foreground">{label}</p>
    </div>
  );
}

interface RowProps {
  icon: ReactNode;
  title: ReactNode;
  titleText?: string;
  detail?: string;
  meta: ReactNode;
  action: ReactNode;
}

function Row({ icon, title, titleText, detail, meta, action }: RowProps) {
  return (
    <li className="flex items-start gap-2 rounded-lg border border-border bg-background p-2.5">
      <span className="mt-0.5 shrink-0">{icon}</span>
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13px] font-medium" title={titleText}>
          {title}
        </div>
        {detail && <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{detail}</p>}
        <div className="mt-1 text-xs text-muted-foreground">{meta}</div>
      </div>
      {action && <div className="-my-1 -mr-1 shrink-0">{action}</div>}
    </li>
  );
}

function StopButton({ label, stopping, onClick }: { label: string; stopping: boolean; onClick: () => void }) {
  const { t } = useTranslation();
  return (
    <Button
      size="icon"
      variant="ghost"
      className="text-destructive hover:bg-destructive/10 hover:text-destructive"
      onClick={onClick}
      disabled={stopping}
      aria-label={stopping ? t('inspector.stopping') : label}
      title={stopping ? t('inspector.stopping') : label}
    >
      {stopping ? <Loader2 className="animate-spin" /> : <Square className="fill-current" />}
    </Button>
  );
}

function JobStatus({ job, stopping }: { job: BackgroundJob; stopping: boolean }) {
  const { t } = useTranslation();
  if (stopping) return <span>{t('inspector.stopping')}</span>;
  switch (job.status.state) {
    case 'running':
      return (
        <span className="inline-flex items-center gap-1 text-success">
          <span className="size-1.5 rounded-full bg-current motion-safe:animate-pulse" aria-hidden />
          {t('inspector.jobRunning')}
        </span>
      );
    case 'completed':
      return (
        <span className={cn(job.status.exit_code !== 0 && 'text-destructive')}>
          {t('inspector.jobExited', { code: job.status.exit_code })}
        </span>
      );
    case 'failed':
      return (
        <span className="text-destructive" title={job.status.error}>
          {t('inspector.jobFailed')}
        </span>
      );
    case 'killed':
      return <span>{t('inspector.jobKilled')}</span>;
  }
}

function PlanMenu({
  sessionId,
  planState,
  content,
}: {
  sessionId: string;
  planState: SessionPlanState;
  content: string;
}) {
  const { t } = useTranslation();
  const updateRuntime = useSessionsStore((s) => s.updateRuntime);
  const [editing, setEditing] = useState(false);
  const [discarding, setDiscarding] = useState(false);

  const transition = async (next: SessionPlanState) => {
    try {
      await putSessionPlanState(sessionId, next);
      updateRuntime(sessionId, { planState: next });
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
      throw err;
    }
  };

  const itemClass =
    'flex h-8 cursor-default select-none items-center gap-2 rounded-md px-2 text-[13px] outline-none data-[highlighted]:bg-accent touch:h-11';

  return (
    <>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <Button size="icon" variant="ghost" className="-my-1.5 -mr-2" aria-label={t('inspector.planActions')}>
            <MoreHorizontal />
          </Button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content
            align="end"
            sideOffset={4}
            className="z-50 min-w-48 rounded-lg border border-border bg-popover p-1 text-popover-foreground shadow-lg"
          >
            <DropdownMenu.Item className={itemClass} onSelect={() => setEditing(true)}>
              <Pencil className="size-4" aria-hidden />
              {t('inspector.editTasks')}…
            </DropdownMenu.Item>
            {planState === 'approved' && (
              <DropdownMenu.Item className={itemClass} onSelect={() => void transition('draft').catch(() => undefined)}>
                <Undo2 className="size-4" aria-hidden />
                {t('inspector.reviseDraft')}
              </DropdownMenu.Item>
            )}
            {planState !== 'disabled' && (
              <>
                <DropdownMenu.Separator className="my-1 h-px bg-border" />
                <DropdownMenu.Item
                  className={cn(itemClass, 'text-destructive data-[highlighted]:bg-destructive/10')}
                  onSelect={() => setDiscarding(true)}
                >
                  <Trash2 className="size-4" aria-hidden />
                  {t('chat.discardPlan')}…
                </DropdownMenu.Item>
              </>
            )}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
      <PlanEditorSheet sessionId={sessionId} open={editing} onOpenChange={setEditing} content={content} />
      <ConfirmDialog
        open={discarding}
        onOpenChange={setDiscarding}
        title={t('plan.discardTitle')}
        description={t('plan.discardBody')}
        confirmLabel={t('chat.discardPlan')}
        destructive
        onConfirm={() => transition('disabled')}
      />
    </>
  );
}

function Waiting({ label }: { label: string }) {
  return (
    <div role="status" className="flex items-center gap-2 text-[13px] text-muted-foreground">
      <Loader2 className="size-4 animate-spin" aria-hidden />
      {label}
    </div>
  );
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-[13px] text-muted-foreground">{children}</p>;
}

function ErrorLine({ message }: { message: string }) {
  const { t } = useTranslation();
  return <p className="text-[13px] text-destructive">{t('inspector.loadFailed', { message })}</p>;
}
