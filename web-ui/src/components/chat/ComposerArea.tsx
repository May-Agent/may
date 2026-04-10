import type { ChangeEvent, ClipboardEvent, RefObject } from 'react';
import { ActivityRail } from './ActivityRail';
import { Composer } from './Composer';
import {
  AskUserPrompt,
  PermissionPrompt,
  WorkspacePrompt,
  type AskUserState,
} from './PromptBanners';
import type {
  AskUserAnswer,
  PermissionRequest,
  WorkspaceRequest,
} from '@/lib/api';
import type { Attachment, PendingUserMessage, TerminalExecution } from '@/types';

export interface ComposerAreaProps {
  // Approval prompts
  pendingPermission: PermissionRequest | null;
  pendingWorkspace: WorkspaceRequest | null;
  pendingAskUser: AskUserState | null;
  isPermissionSubmitting: boolean;
  isWorkspaceSubmitting: boolean;
  isAskUserSubmitting: boolean;
  onPermissionDecision: (approved: boolean) => void;
  onWorkspaceDecision: (approved: boolean) => void;
  onAskUserAnswerChange: (
    questionIndex: number,
    value: AskUserAnswer['answer'],
  ) => void;
  onAskUserSubmit: () => void;
  onAskUserSkipAll: () => void;

  // Error banner
  uiError: string | null;

  // Activity rail
  sessionId: string | null;
  queuedMessages: PendingUserMessage[];
  runningCommands: TerminalExecution[];
  backgroundCommand: {
    taskId?: string | null;
    outputFile?: string | null;
    outputPreview?: string | null;
    inputNotice?: string | null;
  } | null;
  onSteerMessage: (index: number) => void;
  onRemoveMessage: (index: number) => void;

  // Composer
  inputValue: string;
  onInputChange: (value: string) => void;
  placeholder: string;
  attachments: Attachment[];
  onRemoveAttachment: (id: string) => void;
  onPaste: (event: ClipboardEvent) => void;
  onFileSelect: (event: ChangeEvent<HTMLInputElement>) => void;
  fileInputRef: RefObject<HTMLInputElement | null>;
  composerInputRef: RefObject<HTMLTextAreaElement | null>;
  isNewChat: boolean;
  canSend: boolean;
  onSend: () => void;
  onScreenshot: () => void;
  currentModelId: string;
  onModelChange: (id: string) => void;
  isModelUpdating: boolean;
  isSending: boolean;
  currentModelSupportsReasoning: boolean;
  currentReasoningEffort: string | undefined;
  onReasoningEffortChange: (effort: string | undefined) => void;
  showTokenUsage: boolean;
  promptTokens?: number;
  contextUsedTokens?: number;
  contextWindow?: number;
  contextUsagePercent: number | null;
  contextUsageTitle: string;
  showStopButton: boolean;
  showSendButton: boolean;
  onStop: () => void;
}

/**
 * Bottom-of-chat composer area. Wraps the approval prompts, error
 * banner, activity rail, and the composer itself in the sticky
 * pointer-events-none layer so the chat surface above can stay
 * scrollable through the transparent gutter on each side.
 */
export function ComposerArea(props: ComposerAreaProps) {
  return (
    <div className="shrink-0 px-4 pb-2 pt-2 bg-transparent w-full z-20 pointer-events-none">
      <div className="max-w-3xl mx-auto w-full pointer-events-auto">
        {props.pendingPermission && (
          <PermissionPrompt
            request={props.pendingPermission}
            isSubmitting={props.isPermissionSubmitting}
            onDecide={props.onPermissionDecision}
          />
        )}
        {props.pendingWorkspace && (
          <WorkspacePrompt
            request={props.pendingWorkspace}
            isSubmitting={props.isWorkspaceSubmitting}
            onDecide={props.onWorkspaceDecision}
          />
        )}
        {props.pendingAskUser && (
          <AskUserPrompt
            state={props.pendingAskUser}
            isSubmitting={props.isAskUserSubmitting}
            onAnswerChange={props.onAskUserAnswerChange}
            onSubmit={props.onAskUserSubmit}
            onSkipAll={props.onAskUserSkipAll}
          />
        )}
        {props.uiError && (
          <div className="mb-3 rounded-lg border border-destructive bg-destructive px-3 py-2 text-sm text-destructive-foreground">
            {props.uiError}
          </div>
        )}
        <ActivityRail
          pendingMessages={props.queuedMessages}
          runningCommands={props.runningCommands}
          backgroundCommand={props.backgroundCommand}
          sessionId={props.sessionId}
          onSteerMessage={props.onSteerMessage}
          onRemoveMessage={props.onRemoveMessage}
        />
        <Composer
          inputValue={props.inputValue}
          onInputChange={props.onInputChange}
          placeholder={props.placeholder}
          attachments={props.attachments}
          onRemoveAttachment={props.onRemoveAttachment}
          onPaste={props.onPaste}
          onFileSelect={props.onFileSelect}
          fileInputRef={props.fileInputRef}
          composerInputRef={props.composerInputRef}
          isNewChat={props.isNewChat}
          canSend={props.canSend}
          onSend={props.onSend}
          onScreenshot={props.onScreenshot}
          currentModelId={props.currentModelId}
          onModelChange={props.onModelChange}
          isModelUpdating={props.isModelUpdating}
          isSending={props.isSending}
          currentModelSupportsReasoning={props.currentModelSupportsReasoning}
          currentReasoningEffort={props.currentReasoningEffort}
          onReasoningEffortChange={props.onReasoningEffortChange}
          showTokenUsage={props.showTokenUsage}
          currentSessionId={props.sessionId}
          promptTokens={props.promptTokens}
          contextUsedTokens={props.contextUsedTokens}
          contextWindow={props.contextWindow}
          contextUsagePercent={props.contextUsagePercent}
          contextUsageTitle={props.contextUsageTitle}
          showStopButton={props.showStopButton}
          showSendButton={props.showSendButton}
          onStop={props.onStop}
        />
      </div>
    </div>
  );
}
