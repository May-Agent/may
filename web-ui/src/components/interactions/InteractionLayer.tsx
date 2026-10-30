import { useSessionsStore } from '@/lib/stores/sessions';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { PermissionModal } from './PermissionModal';
import { WorkspaceModal } from './WorkspaceModal';
import { AskUserModal, OpenQuestions } from './AskUserModal';

export function InteractionLayer() {
  const active = useSessionsStore((s) => s.active);
  const streamedRuntime = useRuntimeStore((s) => (active ? s.runtimes[active.id] : undefined));
  if (!active) return null;
  const runtime = streamedRuntime ?? active.runtime ?? {};

  return (
    <>
      <PermissionModal request={runtime.waitingPermission ?? null} />
      <WorkspaceModal request={runtime.waitingWorkspace ?? null} />
      <AskUserModal request={runtime.waitingAskUser ?? null} />
      <OpenQuestions requests={runtime.openQuestions ?? []} />
    </>
  );
}
