import type { ContainerPrepareStatus } from '@/lib/api';
import type { ProfileInfo } from '@/types';

// ── API Error Codes (must match backend ApiErrorCode) ────

/** Error codes produced by the backend's typed error responses. */
export type ApiErrorCodeString =
  | 'backend_unavailable'
  | 'container_preparing'
  | 'container_build_failed'
  | 'container_runtime_unavailable'
  | 'managed_podman_approval_required'
  | 'bad_request'
  | 'internal_error'
  | 'not_found';

export const BACKEND_UNAVAILABLE_SUMMARY =
  'Backend unavailable. Unable to load workspace state; please restart server and retry.';

export interface ContainerPreparationState {
  isContainerProfile: boolean;
  isPrepareInProgress: boolean;
  isPrepareFailed: boolean;
  showPreparationGate: boolean;
  summary: string;
}

export type WorkspaceSurfaceState =
  | 'backend-down'
  | 'container-preparing'
  | 'profile-loading'
  | 'ready';

interface ResolveContainerPreparationInput {
  profile: ProfileInfo | null;
  status?: ContainerPrepareStatus;
  /** Human-readable error message from the profile-sessions request. */
  blockedReason?: string | null;
  /** Machine-readable error code from the backend (e.g. `container_preparing`). */
  blockedCode?: string | null;
  backendUnavailableReason?: string | null;
  profileSessionsLoading: boolean;
}

interface ResolveWorkspaceSurfaceInput extends ResolveContainerPreparationInput {
  loaded: boolean;
}

export function isManagedPodmanApprovalCode(code: string | null | undefined): boolean {
  return code === 'managed_podman_approval_required';
}

export function resolveContainerPreparationState(
  input: ResolveContainerPreparationInput,
): ContainerPreparationState {
  const blockedReason = input.blockedReason?.trim() ?? '';
  const blockedCode = input.blockedCode ?? null;
  const hasBackendUnavailableReason =
    (input.backendUnavailableReason?.trim().length ?? 0) > 0 ||
    blockedCode === 'backend_unavailable';
  const isContainerProfile = input.profile?.environment.type === 'container';

  const hasPreparingBlockedCode = blockedCode === 'container_preparing';
  const isPrepareInProgress =
    !hasBackendUnavailableReason &&
    (input.status?.stage === 'building' || hasPreparingBlockedCode);

  const isPrepareFailed =
    hasBackendUnavailableReason ||
    input.status?.stage === 'failed' ||
    blockedCode === 'container_build_failed' ||
    blockedCode === 'container_runtime_unavailable' ||
    blockedCode === 'managed_podman_approval_required' ||
    (blockedReason.length > 0 &&
      !input.profileSessionsLoading &&
      !hasPreparingBlockedCode &&
      !isPrepareInProgress);

  const rawSummary = hasBackendUnavailableReason
    ? input.backendUnavailableReason?.trim() || blockedReason || BACKEND_UNAVAILABLE_SUMMARY
    : isPrepareFailed && blockedReason.length > 0
      ? blockedReason
      : input.status?.summary || 'Preparing workspace container environment';

  return {
    isContainerProfile,
    isPrepareInProgress,
    isPrepareFailed,
    showPreparationGate: isContainerProfile && (isPrepareInProgress || isPrepareFailed),
    summary: rawSummary.trim(),
  };
}

export function resolveWorkspaceSurfaceState(
  input: ResolveWorkspaceSurfaceInput,
): {
  state: WorkspaceSurfaceState;
  container: ContainerPreparationState;
} {
  const container = resolveContainerPreparationState(input);
  const hasBackendUnavailable = (input.backendUnavailableReason?.trim().length ?? 0) > 0;

  if (hasBackendUnavailable) {
    return { state: 'backend-down', container };
  }

  if (container.showPreparationGate) {
    return { state: 'container-preparing', container };
  }

  if (!input.loaded || input.profileSessionsLoading) {
    return { state: 'profile-loading', container };
  }

  return { state: 'ready', container };
}
