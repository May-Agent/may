import type { Dispatch, SetStateAction } from 'react';
import { useCallback, useState } from 'react';
import {
  respondAskUserRequest,
  respondPermissionRequest,
  respondWorkspaceRequest,
  type AskUserAnswer,
  type PermissionRequest,
  type WorkspaceRequest,
} from '@/lib/api';
import { useRuntimeStore } from '@/lib/runtimeStore';
import type { AskUserState } from '@/components/chat/PromptBanners';

export interface UseApprovalHandlersInput {
  currentSessionId: string | null;
  pendingPermission: PermissionRequest | null;
  pendingWorkspace: WorkspaceRequest | null;
  pendingAskUser: AskUserState | null;
  setPendingAskUser: Dispatch<SetStateAction<AskUserState | null>>;
  setError: Dispatch<SetStateAction<string | null>>;
}

export interface UseApprovalHandlersResult {
  isPermissionSubmitting: boolean;
  isWorkspaceSubmitting: boolean;
  isAskUserSubmitting: boolean;
  onPermissionDecision: (approved: boolean) => Promise<void>;
  onWorkspaceDecision: (approved: boolean) => Promise<void>;
  onAskUserAnswerChange: (
    questionIndex: number,
    value: AskUserAnswer['answer'],
  ) => void;
  onAskUserSubmit: (answersOverride?: AskUserAnswer[]) => Promise<void>;
}

/**
 * Centralizes the three approval flows (permission, workspace, ask-user)
 * into a single hook. Each flow follows the same pattern: set submitting,
 * POST the response, apply the returned runtime, surface errors, clear
 * submitting in finally. The hook also owns the submitting booleans.
 */
export function useApprovalHandlers({
  currentSessionId,
  pendingPermission,
  pendingWorkspace,
  pendingAskUser,
  setPendingAskUser,
  setError,
}: UseApprovalHandlersInput): UseApprovalHandlersResult {
  const [isPermissionSubmitting, setIsPermissionSubmitting] = useState(false);
  const [isWorkspaceSubmitting, setIsWorkspaceSubmitting] = useState(false);
  const [isAskUserSubmitting, setIsAskUserSubmitting] = useState(false);
  const applyRuntime = useRuntimeStore((state) => state.applyRuntime);

  const onPermissionDecision = useCallback(
    async (approved: boolean): Promise<void> => {
      if (!pendingPermission || !currentSessionId) return;
      setIsPermissionSubmitting(true);
      try {
        const runtime = await respondPermissionRequest(
          pendingPermission.id,
          approved,
        );
        applyRuntime(currentSessionId, runtime);
      } catch (error) {
        setError(
          `permission submit failed: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      } finally {
        setIsPermissionSubmitting(false);
      }
    },
    [applyRuntime, currentSessionId, pendingPermission, setError],
  );

  const onWorkspaceDecision = useCallback(
    async (approved: boolean): Promise<void> => {
      if (!pendingWorkspace || !currentSessionId) return;
      setIsWorkspaceSubmitting(true);
      try {
        const runtime = await respondWorkspaceRequest(
          pendingWorkspace.id,
          approved,
        );
        applyRuntime(currentSessionId, runtime);
      } catch (error) {
        setError(
          `workspace submit failed: ${
            error instanceof Error ? error.message : String(error)
          }`,
        );
      } finally {
        setIsWorkspaceSubmitting(false);
      }
    },
    [applyRuntime, currentSessionId, pendingWorkspace, setError],
  );

  const onAskUserAnswerChange = useCallback(
    (questionIndex: number, value: AskUserAnswer['answer']): void => {
      setPendingAskUser((prev) => {
        if (!prev) return prev;
        const nextAnswers = [...prev.answers];
        nextAnswers[questionIndex] = {
          section: prev.request.questions[questionIndex]?.section ?? '',
          answer: value,
        };
        return { ...prev, answers: nextAnswers };
      });
    },
    [setPendingAskUser],
  );

  const onAskUserSubmit = useCallback(
    async (answersOverride?: AskUserAnswer[]): Promise<void> => {
      if (!pendingAskUser || !currentSessionId) return;
      setIsAskUserSubmitting(true);
      setError(null);
      try {
        const runtime = await respondAskUserRequest(
          pendingAskUser.request.id,
          answersOverride ?? pendingAskUser.answers,
        );
        applyRuntime(currentSessionId, runtime);
        setPendingAskUser(null);
      } catch (error) {
        setError(`ask_user submit failed: ${(error as Error).message}`);
      } finally {
        setIsAskUserSubmitting(false);
      }
    },
    [
      applyRuntime,
      currentSessionId,
      pendingAskUser,
      setError,
      setPendingAskUser,
    ],
  );

  return {
    isPermissionSubmitting,
    isWorkspaceSubmitting,
    isAskUserSubmitting,
    onPermissionDecision,
    onWorkspaceDecision,
    onAskUserAnswerChange,
    onAskUserSubmit,
  };
}
