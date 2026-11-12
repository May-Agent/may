import { useEffect } from 'react';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useInspectorStore } from '@/lib/stores/inspector';
import { useTurnStore } from '@/lib/stores/turn';
import { PermissionModal } from './PermissionModal';
import { WorkspaceModal } from './WorkspaceModal';
import { AskUserModal, OpenQuestions } from './AskUserModal';

export function InteractionLayer() {
  const active = useSessionsStore((s) => s.active);
  const streamedRuntime = useRuntimeStore((s) => (active ? s.runtimes[active.id] : undefined));
  const loadTasks = useInspectorStore((s) => s.loadTasks);
  const rows = useInspectorStore((s) => (active ? s.sessions[active.id]?.tasks?.rows : undefined));
  const todo = useTurnStore((s) => (active ? s.turns[active.id]?.active?.todo : null) ?? null);
  const runtime = active ? (streamedRuntime ?? active.runtime ?? {}) : {};
  const open = runtime.openQuestions ?? [];
  const activeId = active?.id;

  // The rows an open question blocks come from tasks.md: read it while a
  // question is open and again whenever a turn reports a change to it.
  useEffect(() => {
    if (activeId && open.length > 0) void loadTasks(activeId);
  }, [activeId, open.length, loadTasks, todo]);

  if (!active) return null;

  return (
    <>
      <PermissionModal request={runtime.waitingPermission ?? null} />
      <WorkspaceModal request={runtime.waitingWorkspace ?? null} />
      <AskUserModal request={runtime.waitingAskUser ?? null} />
      <OpenQuestions requests={open} rows={rows ?? []} />
    </>
  );
}
