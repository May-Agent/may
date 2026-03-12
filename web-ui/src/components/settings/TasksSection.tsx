import { useEffect, useMemo, useState } from 'react';
import { Clock3, Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Textarea } from '@/components/ui/textarea';
import { useStore } from '@/lib/store';
import type { CreateTaskInput, ScheduledTask, TaskTriggerKind, TimeTriggerType } from '@/types';
import {
  TASK_TRIGGER_KINDS,
  TIME_TRIGGER_TYPES,
} from './constants';

export function TasksSection() {
  const {
    scheduledTasks,
    refreshTasks,
    createTask,
    updateTask,
    deleteTask,
    approveTask,
    runTask,
    profiles,
    currentProfileId,
  } = useStore();

  const [taskName, setTaskName] = useState('');
  const [taskTriggerKind, setTaskTriggerKind] = useState<TaskTriggerKind>('time');
  const [taskTriggerType, setTaskTriggerType] = useState<TimeTriggerType>('once');
  const [taskEventName, setTaskEventName] = useState('');
  const [taskRunAt, setTaskRunAt] = useState('');
  const [taskCron, setTaskCron] = useState('0 6 * * *');
  const [taskScriptPath, setTaskScriptPath] = useState('');
  const [taskScriptInline, setTaskScriptInline] = useState('');
  const [taskNeedsNetwork, setTaskNeedsNetwork] = useState(false);
  const [taskProfileOverrideId, setTaskProfileOverrideId] = useState<string | null>(null);
  const [taskFormError, setTaskFormError] = useState<string | null>(null);

  useEffect(() => {
    void refreshTasks();
  }, [refreshTasks]);

  const defaultTaskProfileId = currentProfileId ?? profiles[0]?.id ?? '';
  const taskProfileId = useMemo(() => {
    if (!taskProfileOverrideId) {
      return defaultTaskProfileId;
    }

    return profiles.some((profile) => profile.id === taskProfileOverrideId)
      ? taskProfileOverrideId
      : defaultTaskProfileId;
  }, [defaultTaskProfileId, profiles, taskProfileOverrideId]);

  const sortedTasks = useMemo(
    () =>
      [...scheduledTasks].sort(
        (a, b) =>
          new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
      ),
    [scheduledTasks],
  );

  const describeTaskTrigger = (task: ScheduledTask): string => {
    switch (task.triggerKind) {
      case 'time':
        switch (task.triggerType) {
          case 'cron':
            return task.cronExpression ?? 'cron';
          case 'once':
            return task.runAt ?? 'once';
          default:
            return task.triggerType;
        }
      case 'event':
        return task.triggerType;
      case 'webhook':
        return task.webhookPath ?? task.triggerType;
      default:
        return `${task.triggerKind}:${task.triggerType}`;
    }
  };

  const clearTaskForm = () => {
    setTaskName('');
    setTaskTriggerKind('time');
    setTaskEventName('');
    setTaskRunAt('');
    setTaskCron('0 6 * * *');
    setTaskScriptPath('');
    setTaskScriptInline('');
    setTaskNeedsNetwork(false);
    setTaskProfileOverrideId(null);
    setTaskTriggerType('once');
    setTaskFormError(null);
  };

  const handleCreateTask = async () => {
    setTaskFormError(null);
    if (!taskProfileId) {
      setTaskFormError('Please select a profile');
      return;
    }
    if (!taskName.trim()) {
      setTaskFormError('Task name is required');
      return;
    }
    if (!taskScriptPath.trim() && !taskScriptInline.trim()) {
      setTaskFormError('Either script path or inline script is required');
      return;
    }
    if (taskScriptPath.trim() && taskScriptInline.trim()) {
      setTaskFormError('Cannot specify both script path and inline script');
      return;
    }
    if (taskTriggerKind === 'time' && taskTriggerType === 'once' && !taskRunAt.trim()) {
      setTaskFormError('One-time tasks must have a UTC time');
      return;
    }
    if (taskTriggerKind === 'time' && taskTriggerType === 'cron' && !taskCron.trim()) {
      setTaskFormError('Cron tasks must have a cron expression');
      return;
    }
    if (taskTriggerKind === 'event' && !taskEventName.trim()) {
      setTaskFormError('Event-triggered tasks must have an event name');
      return;
    }

    try {
      const baseInput = {
        profileId: taskProfileId,
        name: taskName.trim(),
        scriptPath: taskScriptPath.trim() || undefined,
        scriptInline: taskScriptInline.trim() || undefined,
        needsNetwork: taskNeedsNetwork,
      };
      const taskInput: CreateTaskInput = taskTriggerKind === 'time'
        ? {
            ...baseInput,
            triggerKind: 'time',
            triggerType: taskTriggerType,
            runAt: taskTriggerType === 'once' ? taskRunAt.trim() : undefined,
            cronExpression: taskTriggerType === 'cron' ? taskCron.trim() : undefined,
          }
        : taskTriggerKind === 'event'
          ? {
              ...baseInput,
              triggerKind: 'event',
              eventName: taskEventName.trim(),
            }
          : {
              ...baseInput,
              triggerKind: 'webhook',
            };
      await createTask(taskInput);
      clearTaskForm();
    } catch (e) {
      setTaskFormError((e as Error).message);
    }
  };

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <Clock3 className="h-4 w-4" /> Tasks
      </div>

      <div className="rounded border p-4 space-y-3">
        <div className="grid gap-3 md:grid-cols-2">
          <div className="space-y-1">
            <Label>Profile</Label>
            <Select value={taskProfileId} onValueChange={setTaskProfileOverrideId}>
              <SelectTrigger>
                <SelectValue placeholder="Select profile" />
              </SelectTrigger>
              <SelectContent>
                {profiles.map((profile) => (
                  <SelectItem key={profile.id} value={profile.id}>
                    {profile.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <div className="space-y-1">
            <Label>Name</Label>
            <Input
              value={taskName}
              onChange={(e) => setTaskName(e.target.value)}
              placeholder="Daily report"
            />
          </div>

          <div className="space-y-1">
            <Label>Trigger Kind</Label>
            <Select
              value={taskTriggerKind}
              onValueChange={(v) => setTaskTriggerKind(v as TaskTriggerKind)}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {TASK_TRIGGER_KINDS.map((kind) => (
                  <SelectItem key={kind.value} value={kind.value}>
                    {kind.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          {taskTriggerKind === 'time' && (
            <div className="space-y-1">
              <Label>Trigger</Label>
              <Select
                value={taskTriggerType}
                onValueChange={(v) => setTaskTriggerType(v as TimeTriggerType)}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {TIME_TRIGGER_TYPES.map((kind) => (
                    <SelectItem key={kind.value} value={kind.value}>
                      {kind.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}

          {taskTriggerKind === 'time' && taskTriggerType === 'once' && (
            <div className="space-y-1">
              <Label>Run At (UTC ISO8601)</Label>
              <Input
                value={taskRunAt}
                onChange={(e) => setTaskRunAt(e.target.value)}
                placeholder="2026-02-15T06:00:00Z"
              />
            </div>
          )}

          {taskTriggerKind === 'time' && taskTriggerType === 'cron' && (
            <div className="space-y-1">
              <Label>Cron Expression</Label>
              <Input
                value={taskCron}
                onChange={(e) => setTaskCron(e.target.value)}
                placeholder="0 6 * * *"
              />
            </div>
          )}

          {taskTriggerKind === 'event' && (
            <div className="space-y-1">
              <Label>Event Name</Label>
              <Input
                value={taskEventName}
                onChange={(e) => setTaskEventName(e.target.value)}
                placeholder="chat.turn.completed"
              />
            </div>
          )}

          {taskTriggerKind === 'webhook' && (
            <div className="rounded border bg-muted/20 px-3 py-2 text-xs text-muted-foreground md:col-span-2">
              Creating a webhook task generates a private trigger URL. After creation, send a POST
              request to that path to run the task.
            </div>
          )}

          <div className="space-y-1 md:col-span-2">
            <Label>Script Path</Label>
            <Input
              value={taskScriptPath}
              onChange={(e) => setTaskScriptPath(e.target.value)}
              placeholder="/absolute/path/to/script.sh"
            />
          </div>

          <div className="space-y-1 md:col-span-2">
            <Label>Inline Script</Label>
            <Textarea
              value={taskScriptInline}
              onChange={(e) => setTaskScriptInline(e.target.value)}
              placeholder="#!/usr/bin/env bash"
              className="min-h-[120px] font-mono text-xs"
            />
          </div>
        </div>

        <div className="flex items-center gap-3">
          <input
            type="checkbox"
            checked={taskNeedsNetwork}
            onChange={(e) => setTaskNeedsNetwork(e.target.checked)}
          />
          <span className="text-xs text-muted-foreground">
            Leash task requires network access
          </span>
        </div>

        {taskFormError && (
          <div className="text-xs text-destructive">{taskFormError}</div>
        )}

        <div className="flex gap-2">
          <Button onClick={() => void handleCreateTask()}>
            <Plus className="h-4 w-4 mr-2" />
            Create Task
          </Button>
          <Button variant="outline" onClick={clearTaskForm}>
            Reset
          </Button>
          <Button variant="outline" onClick={() => void refreshTasks()}>
            Reload
          </Button>
        </div>
      </div>

      <div className="rounded border divide-y max-h-[520px] overflow-auto">
        {sortedTasks.length === 0 && (
          <div className="px-3 py-3 text-sm text-muted-foreground">
            No tasks yet.
          </div>
        )}
        {sortedTasks.map((task) => (
          <div key={task.id} className="px-3 py-3 space-y-2">
            <div className="flex items-center justify-between gap-3">
              <div>
                <div className="font-medium">{task.name}</div>
                <div className="text-xs text-muted-foreground font-mono">
                  {describeTaskTrigger(task)}
                </div>
              </div>
              <div className="text-xs text-muted-foreground">
                {task.enabled ? 'enabled' : 'disabled'} · {task.approved ? 'approved' : 'pending approval'}
              </div>
            </div>
            <div className="text-xs text-muted-foreground">
              profile={task.profileId} · updated={task.updatedAt}
            </div>
            {task.webhookPath && (
              <div className="rounded border bg-muted/20 p-2 text-xs font-mono break-all">
                POST {task.webhookPath}
              </div>
            )}
            {task.lastRunStatus && (
              <div className="text-xs">
                last run: {task.lastRunStatus} {task.lastRunAt ? `(${task.lastRunAt})` : ''}
              </div>
            )}
            {task.lastRunOutput && (
              <div className="rounded border bg-muted/20 p-2 text-xs whitespace-pre-wrap break-words max-h-40 overflow-auto">
                {task.lastRunOutput}
              </div>
            )}
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => void updateTask(task.id, { enabled: !task.enabled })}
              >
                {task.enabled ? 'Disable' : 'Enable'}
              </Button>
              {!task.approved && (
                <Button size="sm" onClick={() => void approveTask(task.id)}>
                  Approve
                </Button>
              )}
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void runTask(task.id)}
              >
                Run Now
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => void deleteTask(task.id)}
              >
                <Trash2 className="h-3.5 w-3.5 mr-1" />
                Delete
              </Button>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
