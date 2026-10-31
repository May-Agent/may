import { useEffect, useMemo, useState, type ReactNode } from 'react';
import * as DropdownMenu from '@radix-ui/react-dropdown-menu';
import { ChevronRight, Loader2, MoreHorizontal, PanelRightClose, Pencil, X } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { ConfirmDialog } from '@/components/ui/confirm-dialog';
import { RenameDialog } from '@/components/ui/rename-dialog';
import { ComputerPanel } from '@/components/browser/ComputerPanel';
import { useTranslation } from '@/lib/i18n';
import { EMPTY_INSPECTOR_ENTRY, useInspectorStore } from '@/lib/stores/inspector';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTurnStore } from '@/lib/stores/turn';
import { useUiStore } from '@/lib/stores/ui';
import { type InspectorTab, useBrowserViewStore } from '@/lib/stores/browserView';
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
import { IssueLabel, TaskTreeView } from './TaskTreeView';
import { PlanEditorSheet } from './PlanEditorSheet';
import { LibraryPanel } from './LibraryPanel';

type PendingStop =
  | { kind: 'job'; job: BackgroundJob }
  | { kind: 'subagent'; run: SubagentRun }
  | null;

const TABS: Array<{ id: InspectorTab; label: 'inspector.tabDetails' | 'inspector.tabLibrary' | 'inspector.tabComputer' }> = [
  { id: 'details', label: 'inspector.tabDetails' },
  { id: 'library', label: 'inspector.tabLibrary' },
  { id: 'computer', label: 'inspector.tabComputer' },
];

/**
 * The session inspector, in three tabs: Details (the chat's name, its
 * tasks.md task tree and linked issues, subagents and background jobs),
 * Library (files May made) and Computer (the agent's browser screen).
 */
export function InspectorPane({ session, inSheet = false }: { session: ApiSession; inSheet?: boolean }) {
  const { t } = useTranslation();
  const tab = useBrowserViewStore((s) => s.inspectorTab);
  const setTab = useBrowserViewStore((s) => s.setInspectorTab);
  const toggleInspector = useUiStore((s) => s.toggleInspector);
  const setInspectorSheetOpen = useUiStore((s) => s.setInspectorSheetOpen);
  const close = () => (inSheet ? setInspectorSheetOpen(false) : toggleInspector());

  return (
    <div className="flex h-full min-h-0 flex-col bg-sidebar">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-border pl-3 pr-2 touch:h-14">
        <div role="tablist" aria-label={t('inspector.tabsLabel')} className="flex flex-1 items-center gap-0.5">
          {TABS.map(({ id, label }) => (
            <button
              key={id}
              type="button"
              role="tab"
              id={`inspector-tab-${id}`}
              aria-selected={tab === id}
              aria-controls={`inspector-panel-${id}`}
              onClick={() => setTab(id)}
              className={cn(
                'h-7 rounded-md px-2.5 text-[13px] font-medium transition-colors touch:h-11 touch:px-3',
                tab === id
                  ? 'bg-accent text-foreground'
                  : 'text-muted-foreground hover:text-foreground',
              )}
            >
              {t(label)}
            </button>
          ))}
        </div>
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

      <ScrollArea
        className="min-h-0 flex-1"
        role="tabpanel"
        id={`inspector-panel-${tab}`}
        aria-labelledby={`inspector-tab-${tab}`}
      >
        {tab === 'details' && <DetailsPanel session={session} />}
        {tab === 'library' && <LibraryPanel sessionId={session.id} />}
        {tab === 'computer' && <ComputerPanel sessionId={session.id} visible />}
      </ScrollArea>
    </div>
  );
}

function DetailsPanel({ session }: { session: ApiSession }) {
  const { t } = useTranslation();
  const sessionId = session.id;
  const entry = useInspectorStore((s) => s.sessions[sessionId] ?? EMPTY_INSPECTOR_ENTRY);
  const loadTasks = useInspectorStore((s) => s.loadTasks);
  const loadBackground = useInspectorStore((s) => s.loadBackground);
  const killJob = useInspectorStore((s) => s.killJob);
  const stopSubagent = useInspectorStore((s) => s.stopSubagent);
  const rename = useSessionsStore((s) => s.rename);
  const todo = useTurnStore((s) => s.turns[sessionId]?.active?.todo ?? null);
  const turnActive = useTurnStore((s) => Boolean(s.turns[sessionId]?.active));
  const planState: SessionPlanState = session.runtime?.planState ?? 'disabled';

  const [renaming, setRenaming] = useState(false);
  const [pendingStop, setPendingStop] = useState<PendingStop>(null);
  const [finishedOpen, setFinishedOpen] = useState(false);

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
  const runningJobs = jobs.filter(isJobRunning);
  const finishedJobs = jobs.filter((job) => !isJobRunning(job));
  const subagents = entry.background?.subagents ?? [];
  const backgroundFailed = entry.backgroundError !== null && !entry.background;

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

  const jobRow = (job: BackgroundJob) => {
    const running = isJobRunning(job);
    const stopping = entry.stopping.jobs.includes(job.pid);
    const headline = scriptHeadline(job.script);
    return (
      <ListRow
        key={job.pid}
        title={<span className="font-mono text-xs">{headline}</span>}
        titleText={job.script}
        meta={<JobMeta job={job} stopping={stopping} />}
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
  };

  return (
    <div className="divide-y divide-border">
      <section aria-label={t('inspector.session')} className="flex items-start gap-1 px-4 py-3">
        <div className="min-w-0 flex-1">
          <p className="break-words text-[13px] font-medium leading-5">
            {session.title || t('common.untitled')}
          </p>
          <p className="truncate text-xs text-muted-foreground">{session.model}</p>
        </div>
        <Button
          size="icon"
          variant="ghost"
          className="-mr-2 -mt-1.5 shrink-0"
          onClick={() => setRenaming(true)}
          aria-label={t('inspector.renameSession')}
          title={t('inspector.renameSession')}
        >
          <Pencil />
        </Button>
      </section>

      <Section
        title={t('inspector.tasks')}
        meta={tasks && tasks.tree.total > 0 ? `${tasks.tree.completed}/${tasks.tree.total}` : undefined}
        busy={entry.tasksLoading && tasks !== null}
        accessory={
          tasks && (tasks.content.trim().length > 0 || planState !== 'disabled') ? (
            <PlanMenu sessionId={sessionId} planState={planState} content={tasks.content} />
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
              <p className="mb-2 flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
                <span>{t('inspector.linkedIssues')}</span>
                {tasks.tree.roots.map((issue) => (
                  <IssueLabel key={issue.label} issue={issue} />
                ))}
              </p>
            )}
            {tasks.tree.total === 0 ? (
              <Empty>{t('inspector.tasksEmpty')}</Empty>
            ) : (
              <TaskTreeView tree={tasks.tree} />
            )}
          </>
        )}
      </Section>

      <Section
        title={t('inspector.subagents')}
        meta={subagents.length > 0 ? String(subagents.length) : undefined}
      >
        {backgroundFailed ? (
          <ErrorLine message={entry.backgroundError ?? ''} />
        ) : subagents.length === 0 ? (
          <Empty>{t('inspector.subagentsEmpty')}</Empty>
        ) : (
          <ul className="-mx-2">
            {subagents.map((run) => (
              <ListRow
                key={run.runId}
                title={run.agent}
                detail={run.prompt}
                meta={formatElapsed(run.elapsedMs)}
                metaTitle={t('inspector.subagentTokens', {
                  tokens: formatTokens(run.promptTokens + run.completionTokens),
                  requests: run.requests,
                })}
                action={
                  <StopButton
                    label={t('inspector.stopSubagent', { agent: run.agent })}
                    stopping={entry.stopping.subagents.includes(run.runId)}
                    onClick={() => setPendingStop({ kind: 'subagent', run })}
                  />
                }
              />
            ))}
          </ul>
        )}
      </Section>

      <Section
        title={t('inspector.jobs')}
        meta={runningJobs.length > 0 ? String(runningJobs.length) : undefined}
      >
        {backgroundFailed ? (
          <ErrorLine message={entry.backgroundError ?? ''} />
        ) : jobs.length === 0 ? (
          <Empty>{t('inspector.jobsEmpty')}</Empty>
        ) : (
          <>
            {runningJobs.length > 0 && <ul className="-mx-2">{runningJobs.map(jobRow)}</ul>}
            {finishedJobs.length > 0 && (
              <div className={cn(runningJobs.length > 0 && 'mt-1')}>
                <button
                  type="button"
                  onClick={() => setFinishedOpen((v) => !v)}
                  aria-expanded={finishedOpen}
                  className="-mx-2 flex h-7 items-center gap-1 rounded-md px-2 text-xs text-muted-foreground hover:text-foreground touch:h-11"
                >
                  <ChevronRight
                    className={cn('size-3.5 transition-transform', finishedOpen && 'rotate-90')}
                    aria-hidden
                  />
                  {t('inspector.finished')}
                  <span className="tabular-nums">{finishedJobs.length}</span>
                </button>
                {finishedOpen && <ul className="-mx-2">{finishedJobs.map(jobRow)}</ul>}
              </div>
            )}
          </>
        )}
      </Section>

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
  meta?: string;
  busy?: boolean;
  accessory?: ReactNode;
  children: ReactNode;
}

function Section({ title, meta, busy, accessory, children }: SectionProps) {
  return (
    <section aria-label={title} className="px-4 py-3">
      <div className="mb-1.5 flex min-h-7 items-center gap-1.5">
        <h3 className="text-xs font-medium text-muted-foreground">{title}</h3>
        {meta && <span className="text-xs tabular-nums text-muted-foreground/80">{meta}</span>}
        {busy && <Loader2 className="size-3 animate-spin text-muted-foreground" aria-hidden />}
        <div className="ml-auto">{accessory}</div>
      </div>
      {children}
    </section>
  );
}

interface ListRowProps {
  title: ReactNode;
  titleText?: string;
  detail?: string;
  meta: ReactNode;
  metaTitle?: string;
  action: ReactNode;
}

/** A Linear-style list row: title left, muted meta right, then an action. */
function ListRow({ title, titleText, detail, meta, metaTitle, action }: ListRowProps) {
  return (
    <li className="flex min-h-8 items-center gap-2 rounded-md px-2 py-1 hover:bg-accent/60 touch:min-h-11">
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13px]" title={titleText}>
          {title}
        </div>
        {detail && (
          <p className="truncate text-xs text-muted-foreground" title={detail}>
            {detail}
          </p>
        )}
      </div>
      <span className="shrink-0 text-xs tabular-nums text-muted-foreground" title={metaTitle}>
        {meta}
      </span>
      {action && <div className="-my-1 -mr-1 shrink-0">{action}</div>}
    </li>
  );
}

function StopButton({ label, stopping, onClick }: { label: string; stopping: boolean; onClick: () => void }) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={stopping}
      aria-label={stopping ? t('inspector.stopping') : label}
      title={stopping ? t('inspector.stopping') : label}
      className="flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:opacity-60 touch:size-11"
    >
      {stopping ? (
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
      ) : (
        <span className="size-2.5 rounded-[2px] bg-current" aria-hidden />
      )}
    </button>
  );
}

function JobMeta({ job, stopping }: { job: BackgroundJob; stopping: boolean }) {
  const { t } = useTranslation();
  if (stopping) return <>{t('inspector.stopping')}</>;
  const elapsed = formatElapsed(job.elapsedMs);
  switch (job.status.state) {
    case 'running':
      return <>{elapsed}</>;
    case 'completed':
      return job.status.exit_code === 0 ? (
        <>{elapsed}</>
      ) : (
        <span className="text-destructive">{t('inspector.jobExited', { code: job.status.exit_code })}</span>
      );
    case 'failed':
      return (
        <span className="text-destructive" title={job.status.error}>
          {t('inspector.jobFailed')}
        </span>
      );
    case 'killed':
      return <>{t('inspector.jobKilled')}</>;
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
    'flex h-8 cursor-default select-none items-center rounded-md px-2 text-[13px] outline-none data-[highlighted]:bg-accent touch:h-11';

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
            className="z-50 min-w-44 rounded-lg border border-border bg-popover p-1 text-popover-foreground shadow-lg"
          >
            <DropdownMenu.Item className={itemClass} onSelect={() => setEditing(true)}>
              {t('inspector.editTasks')}…
            </DropdownMenu.Item>
            {planState === 'approved' && (
              <DropdownMenu.Item className={itemClass} onSelect={() => void transition('draft').catch(() => undefined)}>
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
      <Loader2 className="size-3.5 animate-spin" aria-hidden />
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
