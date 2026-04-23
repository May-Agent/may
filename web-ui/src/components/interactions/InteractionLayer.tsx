import { useSessionsStore } from '@/lib/stores/sessions';
import { PermissionModal } from './PermissionModal';
import { WorkspaceModal } from './WorkspaceModal';
import { AskUserModal } from './AskUserModal';

export function InteractionLayer() {
  const active = useSessionsStore((s) => s.active);
  if (!active) return null;
  const runtime = active.runtime ?? {};

  return (
    <>
      <PermissionModal request={runtime.waitingPermission ?? null} />
      <WorkspaceModal request={runtime.waitingWorkspace ?? null} />
      <AskUserModal request={runtime.waitingAskUser ?? null} />
    </>
  );
}
