// Factory that builds the streamChat() event handlers over a shared
// mutable context. This used to be ~330 lines of inline callbacks
// inside ChatInterface.handleSend with all the state as closure
// variables — the factory packages them into a single place with
// typed context and explicit entry points.

import type {
  Message,
  TerminalExecution,
  TimelineSegment,
  ToolExecution,
} from '@/types';
import {
  normalizeToolName,
  isWebSearchToolName,
  isWebFetchToolName,
  parseTerminalToolStartArgs,
} from '@/lib/parsers/toolRequest';
import {
  cloneTimelineSegments,
  closeThinkingSegment,
  collectTerminalExecutions,
  collectThinkingText,
} from './streamTimeline';
import { parseToolResultPayload } from '@/components/chat/chatUtils';

export type StreamEventKind = 'reasoning' | 'text' | 'tool' | null;

export interface StreamState {
  streamedText: string;
  timelineSegments: TimelineSegment[];
  activeThinkingSegmentId: string | null;
  lastStreamEventKind: StreamEventKind;
  terminalSegmentByToolId: Map<string, string>;
  toolSegmentByToolId: Map<string, string>;
  completedNormally: boolean;
}

export function createStreamState(): StreamState {
  return {
    streamedText: '',
    timelineSegments: [],
    activeThinkingSegmentId: null,
    lastStreamEventKind: null,
    terminalSegmentByToolId: new Map(),
    toolSegmentByToolId: new Map(),
    completedNormally: false,
  };
}

export interface StreamEventHandlerDeps {
  sessionId: string;
  assistantId: string;
  state: StreamState;
  /** Flush the assembled assistant message into the session store. */
  flushAssistantText: (force?: boolean) => void;
  /** True if the user has requested a stop (abort or UI stop button). */
  wasStoppedByUser: () => boolean;
  /** Used to capture TTFT metrics. */
  markRuntimeFirstToken: (sessionId: string) => void;
  /** Called with the final assistant message on a successful stream completion. */
  onAssistantFinalized: (update: AssistantFinalizeUpdate) => void;
  /** Called when the stream pauses (e.g. browser takeover). */
  onAssistantPaused: (update: AssistantFinalizeUpdate | null) => void;
  /** Called with any stream-level error message (network, protocol, etc). */
  onStreamError: (errorMessage: string) => void;
  /** Todo progress updates are forwarded directly. */
  onTodoUpdate: (event: {
    items: Array<{ text: string; checked: boolean }>;
    total: number;
    completed: number;
  }) => void;
}

export interface AssistantFinalizeUpdate {
  assistant: Message;
  finalContent: string;
  finalThinking: string;
  finalTimelineSegments: TimelineSegment[];
  finalTerminalExecutions: TerminalExecution[];
  isEmpty: boolean;
}

export interface StreamEventHandlers {
  onReasoning: (chunk: string) => void;
  onToolStart: (tool: any) => void;
  onToolEnd: (tool: any) => void;
  onTodoUpdate: (event: {
    items: Array<{ text: string; checked: boolean }>;
    total: number;
    completed: number;
  }) => void;
  onText: (chunk: string) => void;
  onDone: (assistant: Message) => void;
  onPaused: (assistant: Message) => void;
  onError: (message: string) => void;
  closeActiveThinkingSegment: () => void;
}

export function createStreamEventHandlers(
  deps: StreamEventHandlerDeps,
): StreamEventHandlers {
  const {
    state,
    flushAssistantText,
    wasStoppedByUser,
    markRuntimeFirstToken,
    onAssistantFinalized,
    onAssistantPaused,
    onStreamError,
    onTodoUpdate,
    sessionId,
  } = deps;

  const closeActiveThinkingSegment = (): void => {
    if (!state.activeThinkingSegmentId) return;
    state.timelineSegments = closeThinkingSegment(
      state.timelineSegments,
      state.activeThinkingSegmentId,
      Date.now(),
    );
    state.activeThinkingSegmentId = null;
  };

  const onReasoning = (chunk: string): void => {
    const now = Date.now();
    const shouldCreateSegment =
      !state.activeThinkingSegmentId || state.lastStreamEventKind !== 'reasoning';
    if (shouldCreateSegment) {
      const nextId = `thinking-${now}-${Math.random().toString(36).slice(2, 8)}`;
      state.activeThinkingSegmentId = nextId;
      state.timelineSegments = [
        ...state.timelineSegments,
        {
          id: nextId,
          kind: 'thinking',
          offset: state.streamedText.length,
          content: chunk,
          isRunning: true,
          startedAt: now,
        },
      ];
    } else {
      state.timelineSegments = state.timelineSegments.map((segment) =>
        segment.kind === 'thinking' &&
        segment.id === state.activeThinkingSegmentId
          ? {
              ...segment,
              content: `${segment.content}${chunk}`,
              isRunning: true,
            }
          : segment,
      );
    }
    state.lastStreamEventKind = 'reasoning';
    flushAssistantText(true);
  };

  const onToolStart = (tool: any): void => {
    const name = normalizeToolName(tool.name);
    const toolId = tool.id ?? `tool-${Date.now()}`;
    const serializedArguments =
      typeof tool.arguments === 'string'
        ? tool.arguments
        : JSON.stringify(tool.arguments ?? {});

    if (name === 'terminal') {
      appendTerminalToolStart(state, tool, toolId);
      flushAssistantText(true);
      return;
    }

    if (isWebSearchToolName(name) || isWebFetchToolName(name)) {
      closeActiveThinkingSegment();
      const startedAt = Date.now();
      const nextTool: ToolExecution = {
        id: toolId,
        name,
        request: serializedArguments,
        status: 'running',
        startedAt,
      };
      const segmentId = `tool-${toolId}`;
      state.timelineSegments = [
        ...state.timelineSegments,
        {
          id: segmentId,
          kind: 'tool',
          offset: state.streamedText.length,
          tool: nextTool,
        },
      ];
      state.toolSegmentByToolId.set(toolId, segmentId);
      state.lastStreamEventKind = 'tool';
      flushAssistantText(true);
    }
  };

  /** Helper for onToolStart — terminal tools get batched into segments. */
  function appendTerminalToolStart(
    streamState: StreamState,
    tool: any,
    toolId: string,
  ): void {
    const args = parseTerminalToolStartArgs(tool.arguments);
    closeActiveThinkingSegment();
    const startedAt = Date.now();
    const nextExec: TerminalExecution = {
      id: toolId,
      description: args.description,
      input: args.script,
      status: 'running',
      mode: args.mode ?? 'default',
      startedAt,
    };

    const lastSegment =
      streamState.timelineSegments[streamState.timelineSegments.length - 1];
    const canAppendToBatch =
      streamState.lastStreamEventKind === 'tool' &&
      lastSegment?.kind === 'terminal' &&
      lastSegment.offset === streamState.streamedText.length;

    let segmentId: string;
    if (canAppendToBatch && lastSegment.kind === 'terminal') {
      segmentId = lastSegment.id;
      streamState.timelineSegments = streamState.timelineSegments.map(
        (segment) =>
          segment.kind === 'terminal' && segment.id === segmentId
            ? {
                ...segment,
                startedAt: segment.startedAt ?? startedAt,
                endedAt: undefined,
                commands: [...segment.commands, nextExec],
              }
            : segment,
      );
    } else {
      segmentId = `terminal-${toolId}`;
      streamState.timelineSegments = [
        ...streamState.timelineSegments,
        {
          id: segmentId,
          kind: 'terminal',
          offset: streamState.streamedText.length,
          commands: [nextExec],
          startedAt,
        },
      ];
    }

    streamState.terminalSegmentByToolId.set(toolId, segmentId);
    streamState.lastStreamEventKind = 'tool';
  }

  const onToolEnd = (tool: any): void => {
    const toolId = typeof tool?.id === 'string' ? tool.id : '';
    if (!toolId) return;
    const serializedArguments =
      typeof tool?.arguments === 'string'
        ? tool.arguments
        : JSON.stringify(tool?.arguments ?? {});
    const endedAt = Date.now();
    const { output, status } = parseToolResultPayload(tool.result);

    const terminalSegmentId = state.terminalSegmentByToolId.get(toolId);
    if (terminalSegmentId) {
      state.timelineSegments = state.timelineSegments.map((segment) => {
        if (segment.kind !== 'terminal' || segment.id !== terminalSegmentId) {
          return segment;
        }
        const nextCommands = segment.commands.map((command) =>
          command.id === toolId
            ? { ...command, output, status, endedAt }
            : command,
        );
        const runningLeft = nextCommands.some(
          (command) => command.status === 'running',
        );
        return {
          ...segment,
          commands: nextCommands,
          endedAt: runningLeft ? undefined : endedAt,
        };
      });
      state.lastStreamEventKind = 'tool';
      flushAssistantText(true);
      return;
    }

    const toolSegmentId = state.toolSegmentByToolId.get(toolId);
    if (!toolSegmentId) return;
    state.timelineSegments = state.timelineSegments.map((segment) =>
      segment.kind === 'tool' && segment.id === toolSegmentId
        ? {
            ...segment,
            tool: {
              ...segment.tool,
              request: segment.tool.request ?? serializedArguments,
              response: output,
              status,
              endedAt,
            },
          }
        : segment,
    );
    state.lastStreamEventKind = 'tool';
    flushAssistantText(true);
  };

  const onText = (chunk: string): void => {
    closeActiveThinkingSegment();
    state.streamedText += chunk;
    state.lastStreamEventKind = 'text';
    markRuntimeFirstToken(sessionId);
    flushAssistantText();
  };

  const onDone = (assistant: Message): void => {
    closeActiveThinkingSegment();
    const finalTimelineSegments =
      state.timelineSegments.length > 0
        ? cloneTimelineSegments(state.timelineSegments)
        : cloneTimelineSegments(assistant.timelineSegments ?? []);
    const finalContent =
      state.streamedText.trim().length > 0 ? state.streamedText : assistant.content;
    const finalThinking = collectThinkingText(finalTimelineSegments);
    const finalTerminalExecutions = collectTerminalExecutions(
      finalTimelineSegments,
    );
    const isEmpty =
      finalContent.trim().length === 0 &&
      finalThinking.trim().length === 0 &&
      finalTimelineSegments.length === 0;
    state.completedNormally = !isEmpty;

    onAssistantFinalized({
      assistant,
      finalContent,
      finalThinking,
      finalTimelineSegments,
      finalTerminalExecutions,
      isEmpty,
    });
  };

  const onPaused = (assistant: Message): void => {
    closeActiveThinkingSegment();
    state.completedNormally = true;
    const finalTimelineSegments = cloneTimelineSegments(state.timelineSegments);
    const finalThinking = collectThinkingText(finalTimelineSegments);
    const finalTerminalExecutions = collectTerminalExecutions(
      finalTimelineSegments,
    );
    const isEmpty =
      assistant.content.trim().length === 0 &&
      finalThinking.trim().length === 0 &&
      finalTimelineSegments.length === 0;

    if (isEmpty) {
      onAssistantPaused(null);
      return;
    }

    onAssistantPaused({
      assistant,
      finalContent: assistant.content,
      finalThinking,
      finalTimelineSegments,
      finalTerminalExecutions,
      isEmpty: false,
    });
  };

  const onError = (message: string): void => {
    if (wasStoppedByUser()) {
      state.completedNormally = true;
      return;
    }
    closeActiveThinkingSegment();
    onStreamError(message);
  };

  return {
    onReasoning,
    onToolStart,
    onToolEnd,
    onTodoUpdate,
    onText,
    onDone,
    onPaused,
    onError,
    closeActiveThinkingSegment,
  };
}
