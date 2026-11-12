import { useEffect, useState } from 'react';
import {
  Plus,
  Trash2,
  Play,
  Shield,
  CheckCircle2,
  XCircle,
  Clock,
  Radio,
  Webhook,
  Copy,
  ChevronDown,
  ChevronRight,
  CalendarClock,
} from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Badge } from '@/components/ui/badge';
import { Segmented } from '@/components/ui/segmented';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import {
  approveScheduledTask,
  createScheduledTask,
  deleteScheduledTask,
  listScheduledTasks,
  runScheduledTask,
  updateScheduledTask,
} from '@/lib/api/scheduledTasks';
import { listProfiles, type ProfileInfo } from '@/lib/api/profiles';
import type {
  CreateTaskRequest,
  ScheduledTaskResponse,
  TaskRunStatus,
  TaskTriggerRequest,
  TimeTriggerType,
} from '@/lib/types';
import { cn, useFormatRelativeTime } from '@/lib/utils';

type TriggerKind = 'time' | 'event' | 'webhook';

interface DraftState {
  name: string;
  profileId: string;
  kind: TriggerKind;
  timeType: TimeTriggerType;
  cronExpression: string;
  runAt: string;
  eventName: string;
  scriptInline: string;
  scriptPath: string;
  scriptMode: 'inline' | 'path';
  needsNetwork: boolean;
}

const CRON_PRESETS: Array<{ label: string; value: string }> = [
  { label: 'Every hour', value: '0 * * * *' },
  { label: 'Every day, 09:00', value: '0 9 * * *' },
  { label: 'Weekdays, 09:00', value: '0 9 * * 1-5' },
  { label: 'Every Monday', value: '0 9 * * 1' },
];

function emptyDraft(profileId: string): DraftState {
  return {
    name: '',
    profileId,
    kind: 'time',
    timeType: 'cron',
    cronExpression: '0 9 * * *',
    runAt: '',
    eventName: '',
    scriptInline: '',
    scriptPath: '',
    scriptMode: 'inline',
    needsNetwork: false,
  };
}

function buildTriggerRequest(draft: DraftState): TaskTriggerRequest | null {
  if (draft.kind === 'time') {
    if (draft.timeType === 'once') {
      if (!draft.runAt) return null;
      return { kind: 'time', triggerType: 'once', runAtUtc: new Date(draft.runAt).toISOString() };
    }
    if (!draft.cronExpression.trim()) return null;
    return { kind: 'time', triggerType: 'cron', cronUtc: draft.cronExpression.trim() };
  }
  if (draft.kind === 'event') {
    if (!draft.eventName.trim()) return null;
    return { kind: 'event', eventName: draft.eventName.trim() };
  }
  return { kind: 'webhook' };
}

function triggerSummary(task: ScheduledTaskResponse): string {
  const trig = task.trigger;
  if (trig.kind === 'time') {
    if (trig.triggerType === 'cron') return `Cron · ${trig.cronUtc ?? '—'}`;
    if (trig.runAtUtc) return `Once · ${new Date(trig.runAtUtc).toLocaleString()}`;
    return 'Once · —';
  }
  if (trig.kind === 'event') return `Event · ${trig.eventName}`;
  return `Webhook · /api/tasks/webhook/${trig.token}`;
}

function triggerIcon(task: ScheduledTaskResponse) {
  if (task.trigger.kind === 'time') return Clock;
  if (task.trigger.kind === 'event') return Radio;
  return Webhook;
}

const RUN_STATUS_TONE: Record<TaskRunStatus, 'positive' | 'danger' | 'warning' | 'neutral'> = {
  success: 'positive',
  failed: 'danger',
  interrupted: 'warning',
  running: 'neutral',
};

export function TriggersSection() {
  const { t } = useTranslation();
  const formatRelative = useFormatRelativeTime();
  const [tasks, setTasks] = useState<ScheduledTaskResponse[]>([]);
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [draft, setDraft] = useState<DraftState | null>(null);

  const refresh = async () => {
    try {
      const [t, p] = await Promise.all([listScheduledTasks(), listProfiles()]);
      setTasks(t);
      setProfiles(p);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const defaultProfileId = profiles[0]?.id ?? '';

  const startDraft = () => {
    if (!defaultProfileId) {
      toast.error('Create a profile first');
      return;
    }
    setDraft(emptyDraft(defaultProfileId));
  };

  const submitDraft = async () => {
    if (!draft) return;
    const trigger = buildTriggerRequest(draft);
    if (!trigger) {
      toast.error('Please complete the trigger');
      return;
    }
    if (!draft.name.trim()) {
      toast.error('Please name your trigger');
      return;
    }
    const body: CreateTaskRequest = {
      profileId: draft.profileId,
      name: draft.name.trim(),
      trigger,
      needsNetwork: draft.needsNetwork,
      ...(draft.scriptMode === 'inline'
        ? { scriptInline: draft.scriptInline.trim() || null }
        : { scriptPath: draft.scriptPath.trim() || null }),
    };
    setBusy(true);
    try {
      await createScheduledTask(body);
      setDraft(null);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const toggleEnabled = async (task: ScheduledTaskResponse, next: boolean) => {
    try {
      await updateScheduledTask(task.id, { enabled: next });
      setTasks((prev) => prev.map((t) => (t.id === task.id ? { ...t, enabled: next } : t)));
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    }
  };

  const approve = async (task: ScheduledTaskResponse) => {
    setBusy(true);
    try {
      await approveScheduledTask(task.id);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const runNow = async (task: ScheduledTaskResponse) => {
    setBusy(true);
    try {
      await runScheduledTask(task.id);
      toast.success(`Started “${task.name}”`);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const del = async (task: ScheduledTaskResponse) => {
    setBusy(true);
    try {
      await deleteScheduledTask(task.id);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <SectionHeader
        title={t('settings.triggers.title')}
        description={t('settings.triggers.description')}
        action={
          !draft && (
            <Button size="sm" onClick={startDraft}>
              <Plus className="size-3.5" />
              {t('settings.triggers.create')}
            </Button>
          )
        }
      />

      {loading ? (
        <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : (
        <div className="space-y-3">
          {draft && (
            <DraftCard
              draft={draft}
              profiles={profiles}
              onChange={setDraft}
              onSubmit={submitDraft}
              onCancel={() => setDraft(null)}
              busy={busy}
            />
          )}

          {tasks.length === 0 && !draft ? (
            <EmptyState onCreate={startDraft} hasProfile={profiles.length > 0} />
          ) : (
            <ul className="space-y-2">
              {tasks.map((task) => {
                const TIcon = triggerIcon(task);
                const isOpen = expanded === task.id;
                return (
                  <li
                    key={task.id}
                    className="rounded-lg border border-border bg-card/20 transition-colors"
                  >
                    <div className="flex items-center gap-3 px-4 py-3">
                      <Switch
                        checked={task.enabled}
                        onCheckedChange={(v) => toggleEnabled(task, v)}
                      />
                      <TIcon className="size-4 shrink-0 text-muted-foreground" />
                      <button
                        type="button"
                        onClick={() => setExpanded(isOpen ? null : task.id)}
                        className="min-w-0 flex-1 text-left"
                      >
                        <div className="flex items-center gap-2">
                          <span className="truncate text-sm font-medium">{task.name}</span>
                          {!task.approved && (
                            <Badge tone="warning">
                              <Shield className="size-2.5" />
                              needs approval
                            </Badge>
                          )}
                          {task.lastRunStatus && (
                            <Badge tone={RUN_STATUS_TONE[task.lastRunStatus]}>
                              {task.lastRunStatus === 'success' ? (
                                <CheckCircle2 className="size-2.5" />
                              ) : task.lastRunStatus === 'failed' ? (
                                <XCircle className="size-2.5" />
                              ) : null}
                              {task.lastRunStatus}
                            </Badge>
                          )}
                        </div>
                        <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground">
                          {triggerSummary(task)}
                          {task.lastRunAt && (
                            <> · last run {formatRelative(task.lastRunAt)}</>
                          )}
                        </div>
                      </button>
                      {!task.approved && (
                        <Button size="sm" variant="outline" disabled={busy} onClick={() => approve(task)}>
                          {t('settings.triggers.approve')}
                        </Button>
                      )}
                      <Button size="sm" variant="ghost" disabled={busy} onClick={() => runNow(task)}>
                        <Play className="size-3.5" />
                        {t('settings.triggers.run')}
                      </Button>
                      <Button
                        size="icon"
                        variant="ghost"
                        disabled={busy}
                        onClick={() => del(task)}
                        aria-label={t('common.delete')}
                      >
                        <Trash2 className="size-4" />
                      </Button>
                    </div>
                    {isOpen && <EditPanel task={task} onSaved={refresh} onClose={() => setExpanded(null)} />}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}

function EmptyState({ onCreate, hasProfile }: { onCreate: () => void; hasProfile: boolean }) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 rounded-lg border border-dashed border-border bg-card/20 py-14 text-center">
      <CalendarClock className="size-8 text-muted-foreground/40" />
      <div>
        <div className="text-sm font-medium">No triggers yet</div>
        <div className="mt-1 text-xs text-muted-foreground">
          Automate recurring tasks on a schedule, on events, or via webhook.
        </div>
      </div>
      <Button size="sm" variant="outline" onClick={onCreate} disabled={!hasProfile}>
        <Plus className="size-3.5" />
        Schedule your first trigger
      </Button>
    </div>
  );
}

interface DraftCardProps {
  draft: DraftState;
  profiles: ProfileInfo[];
  onChange: (next: DraftState) => void;
  onSubmit: () => void;
  onCancel: () => void;
  busy: boolean;
}

function DraftCard({ draft, profiles, onChange, onSubmit, onCancel, busy }: DraftCardProps) {
  const set = <K extends keyof DraftState>(key: K, value: DraftState[K]) =>
    onChange({ ...draft, [key]: value });

  return (
    <div className="rounded-lg border border-primary/30 bg-card/40 p-4 shadow-sm">
      <div className="grid grid-cols-2 gap-3">
        <Field label="Name">
          <Input
            autoFocus
            value={draft.name}
            onChange={(e) => set('name', e.target.value)}
            placeholder="Morning summary"
          />
        </Field>
        <Field label="Profile">
          <select
            value={draft.profileId}
            onChange={(e) => set('profileId', e.target.value)}
            className="flex h-9 w-full rounded-md border border-border bg-background px-2 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </Field>
      </div>

      <div className="mt-4">
        <Label className="mb-1.5 block">Trigger</Label>
        <Segmented<TriggerKind>
          size="sm"
          value={draft.kind}
          onValueChange={(v) => set('kind', v)}
          options={[
            { value: 'time', icon: <Clock className="size-3" />, label: 'Time' },
            { value: 'event', icon: <Radio className="size-3" />, label: 'Event' },
            { value: 'webhook', icon: <Webhook className="size-3" />, label: 'Webhook' },
          ]}
        />
      </div>

      {draft.kind === 'time' && (
        <div className="mt-3 space-y-3">
          <Segmented<TimeTriggerType>
            size="sm"
            value={draft.timeType}
            onValueChange={(v) => set('timeType', v)}
            options={[
              { value: 'cron', label: 'Recurring' },
              { value: 'once', label: 'Once' },
            ]}
          />
          {draft.timeType === 'cron' ? (
            <div className="space-y-2">
              <div className="flex flex-wrap gap-1.5">
                {CRON_PRESETS.map((p) => (
                  <button
                    key={p.value}
                    type="button"
                    onClick={() => set('cronExpression', p.value)}
                    className={cn(
                      'rounded-md border px-2 py-0.5 text-[11px] transition-colors',
                      draft.cronExpression === p.value
                        ? 'border-primary/40 bg-primary/10 text-primary'
                        : 'border-border text-muted-foreground hover:bg-accent',
                    )}
                  >
                    {p.label}
                  </button>
                ))}
              </div>
              <Input
                value={draft.cronExpression}
                onChange={(e) => set('cronExpression', e.target.value)}
                placeholder="0 9 * * *"
                className="font-mono"
              />
            </div>
          ) : (
            <Input
              type="datetime-local"
              value={draft.runAt}
              onChange={(e) => set('runAt', e.target.value)}
            />
          )}
        </div>
      )}

      {draft.kind === 'event' && (
        <div className="mt-3">
          <Input
            value={draft.eventName}
            onChange={(e) => set('eventName', e.target.value)}
            placeholder="e.g. deploy.finished"
            className="font-mono"
          />
        </div>
      )}

      {draft.kind === 'webhook' && (
        <p className="mt-3 text-xs text-muted-foreground">
          After saving, May will generate a unique webhook URL you can call to trigger this task.
        </p>
      )}

      <div className="mt-4 space-y-2">
        <Label>Script</Label>
        <Segmented<'inline' | 'path'>
          size="sm"
          value={draft.scriptMode}
          onValueChange={(v) => set('scriptMode', v)}
          options={[
            { value: 'inline', label: 'Inline' },
            { value: 'path', label: 'Path' },
          ]}
        />
        {draft.scriptMode === 'inline' ? (
          <Textarea
            value={draft.scriptInline}
            onChange={(e) => set('scriptInline', e.target.value)}
            placeholder="# Prompt for May to run"
            className="font-mono text-xs"
            rows={6}
          />
        ) : (
          <Input
            value={draft.scriptPath}
            onChange={(e) => set('scriptPath', e.target.value)}
            placeholder="~/.may/scripts/daily.md"
            className="font-mono"
          />
        )}
      </div>

      <div className="mt-4 flex items-center justify-between gap-3 rounded-md bg-muted/40 px-3 py-2">
        <div>
          <div className="text-sm font-medium">Needs network</div>
          <div className="text-[11px] text-muted-foreground">
            Requires approval the first time this task runs.
          </div>
        </div>
        <Switch
          checked={draft.needsNetwork}
          onCheckedChange={(v) => set('needsNetwork', v)}
        />
      </div>

      <div className="mt-4 flex justify-end gap-2">
        <Button size="sm" variant="ghost" onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button size="sm" onClick={onSubmit} disabled={busy}>
          Save trigger
        </Button>
      </div>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <Label>{label}</Label>
      {children}
    </div>
  );
}

interface EditPanelProps {
  task: ScheduledTaskResponse;
  onSaved: () => Promise<void>;
  onClose: () => void;
}

function EditPanel({ task, onSaved, onClose }: EditPanelProps) {
  const [name, setName] = useState(task.name);
  const [cron, setCron] = useState(
    task.trigger.kind === 'time' && task.trigger.triggerType === 'cron' ? task.trigger.cronUtc ?? '' : '',
  );
  const [scriptInline, setScriptInline] = useState(task.scriptInline ?? '');
  const [scriptPath, setScriptPath] = useState(task.scriptPath ?? '');
  const [busy, setBusy] = useState(false);
  const [showOutput, setShowOutput] = useState(false);

  const isCron = task.trigger.kind === 'time' && task.trigger.triggerType === 'cron';
  const scriptMode: 'inline' | 'path' = task.scriptPath ? 'path' : 'inline';

  const save = async () => {
    setBusy(true);
    try {
      await updateScheduledTask(task.id, {
        name: name.trim() || null,
        cronExpression: isCron ? cron.trim() || null : null,
        scriptInline: scriptMode === 'inline' ? scriptInline : null,
        scriptPath: scriptMode === 'path' ? scriptPath : null,
      });
      await onSaved();
      onClose();
    } catch (err) {
      toast.error('Failed to save', {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const copyWebhook = async () => {
    if (task.trigger.kind !== 'webhook') return;
    const base = window.location.origin;
    const url = `${base}/api/tasks/webhook/${task.trigger.token}`;
    await navigator.clipboard.writeText(url);
    toast.success('Webhook URL copied');
  };

  return (
    <div className="border-t border-border bg-background/50 px-4 py-4">
      <div className="grid grid-cols-2 gap-3">
        <Field label="Name">
          <Input value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        {isCron && (
          <Field label="Cron expression">
            <Input value={cron} onChange={(e) => setCron(e.target.value)} className="font-mono" />
          </Field>
        )}
      </div>

      <div className="mt-3 space-y-2">
        <Label>Script</Label>
        {scriptMode === 'inline' ? (
          <Textarea
            value={scriptInline}
            onChange={(e) => setScriptInline(e.target.value)}
            className="font-mono text-xs"
            rows={5}
          />
        ) : (
          <Input
            value={scriptPath}
            onChange={(e) => setScriptPath(e.target.value)}
            className="font-mono"
          />
        )}
      </div>

      {task.trigger.kind === 'webhook' && (
        <div className="mt-3 flex items-center gap-2 rounded-md border border-border bg-muted/30 px-3 py-2">
          <code className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted-foreground">
            /api/tasks/webhook/{task.trigger.token}
          </code>
          <Button size="sm" variant="ghost" onClick={copyWebhook}>
            <Copy className="size-3.5" />
            Copy URL
          </Button>
        </div>
      )}

      {task.lastRunOutput && (
        <div className="mt-3">
          <button
            type="button"
            onClick={() => setShowOutput((v) => !v)}
            className="flex items-center gap-1 text-xs font-medium text-muted-foreground hover:text-foreground"
          >
            {showOutput ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
            Last run output
          </button>
          {showOutput && (
            <pre className="mt-2 max-h-48 overflow-auto rounded-md border border-border bg-card/20 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
              {task.lastRunOutput}
            </pre>
          )}
        </div>
      )}

      <div className="mt-4 flex justify-end gap-2">
        <Button size="sm" variant="ghost" onClick={onClose} disabled={busy}>
          Close
        </Button>
        <Button size="sm" onClick={save} disabled={busy}>
          Save changes
        </Button>
      </div>
    </div>
  );
}
