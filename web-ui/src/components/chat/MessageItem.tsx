import { Suspense, lazy, memo, useState, useMemo, useEffect, useRef } from 'react';
import { motion } from 'framer-motion';
import { cn } from '@/lib/utils';
import { fetchSessionTasks, saveSessionTasks } from '@/lib/api';
import type { ArtifactPreviewTarget, Message, SessionPlanState } from '@/types';
import { useStore } from '@/lib/store';
import { extractArtifactPreviewTargets, stripArtifactPreviewTags } from './chatUtils';
import { AttachmentCard } from './AttachmentCard';
import ReactMarkdown from 'react-markdown';
import { isLikelyBinaryTextPayload } from '@/lib/parsers/toolOutput';
import {
  type TimelineDisplayItem,
  buildTimelineDisplayItems,
} from '@/lib/timeline';
import { RotateCcw, Download, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AttachmentPreview } from './AttachmentPreview';
import { CopyButton } from './shared/CopyButton';
import { TimelineItemRow } from './timeline/TimelineItemRow';
import { CommandTimeline } from './timeline/CommandTimeline';
import { ThinkingTimeline } from './timeline/ThinkingTimeline';
import { WebSearchTimeline } from './timeline/WebSearchTimeline';
import { WebFetchTimeline } from './timeline/WebFetchTimeline';
import { BrowserTimeline } from './timeline/BrowserTimeline';

const TASKS_TAG_PATTERN = /<tasks\s*\/>/i;
const LazySyntaxCodeBlock = lazy(() => import('./SyntaxCodeBlock'));
const LazyFlowMarkdown = lazy(() =>
  import('./StreamingText').then((module) => ({ default: module.FlowMarkdown })),
);

function stripTasksTag(content: string): string {
  return content.replace(/<tasks\s*\/>/gi, "").trim();
}

const MemoMarkdown = memo(function MemoMarkdown({
  content,
  components,
}: {
  content: string;
  components: any;
}) {
  return <ReactMarkdown components={components}>{content}</ReactMarkdown>;
});






function makeComponents(isUser: boolean, isStreaming: boolean) {
  return {
    code({ className, children, ...props }: any) {
      const lang = /language-(\w+)/.exec(className || '')?.[1];
      const code = String(children).replace(/\n$/, '');
      return lang ? (
        <div className="relative group/code my-3 rounded-lg overflow-hidden border border-border/30">
          <div className="flex items-center justify-between px-4 py-1.5 bg-muted text-muted-foreground text-xs">
            <span>{lang}</span>
            {!isStreaming && <CopyButton text={code} className="h-5 w-5 text-muted-foreground hover:text-foreground hover:bg-background/80" />}
          </div>
          <Suspense
            fallback={
              <pre className="m-0 overflow-x-auto bg-background px-4 py-3 text-[0.8125rem] text-foreground">
                <code>{code}</code>
              </pre>
            }
          >
            <LazySyntaxCodeBlock language={lang} code={code} />
          </Suspense>
        </div>
      ) : (
        <code {...props} className={cn('rounded px-1 py-0.5 font-mono text-sm', isUser ? 'bg-black/10 dark:bg-white/10' : 'bg-muted', className)}>{children}</code>
      );
    },
    p: ({ children }: any) => <p className="mb-1 last:mb-0 leading-relaxed">{children}</p>,
    ul: ({ children }: any) => <ul className="list-disc list-inside mb-2 last:mb-0 pl-1">{children}</ul>,
    ol: ({ children }: any) => <ol className="list-decimal list-inside mb-2 last:mb-0 pl-1">{children}</ol>,
  };
}

export function MessageItem({
  message,
  sessionId,
  onRetry,
  canRetry,
  onOpenPreview,
}: {
  message: Message;
  sessionId?: string;
  onRetry?: () => void;
  canRetry?: boolean;
  onOpenPreview?: (target: ArtifactPreviewTarget) => void;
}) {
  const isUser = message.role === 'user';
  const isStreaming = !!message.isStreaming;
  const [expandedTimelineItemIds, setExpandedTimelineItemIds] = useState<Record<string, boolean>>({});
  const timelineExpansionOverridesRef = useRef<Record<string, boolean>>({});
  const [showTyping, setShowTyping] = useState(false);
  const [tasksContent, setTasksContent] = useState("");
  const [tasksDraft, setTasksDraft] = useState("");
  const [tasksLoading, setTasksLoading] = useState(false);
  const [tasksSaving, setTasksSaving] = useState(false);
  const [tasksEditing, setTasksEditing] = useState(false);
  const [tasksError, setTasksError] = useState<string | null>(null);
  const [tasksPlanState, setTasksPlanState] = useState<SessionPlanState>('disabled');
  const setSessionPlanState = useStore((state) => state.setSessionPlanState);
  const components = useMemo(
    () => makeComponents(isUser, isStreaming),
    [isUser, isStreaming]
  );
  const showsTasksTag = useMemo(
    () => TASKS_TAG_PATTERN.test(message.content || ""),
    [message.content],
  );

  const previewTargets = useMemo(
    () => extractArtifactPreviewTargets(message.content || ''),
    [message.content],
  );
  const displayContent = useMemo(
    () => stripTasksTag(stripArtifactPreviewTags(message.content || '')),
    [message.content],
  );
  const timelineItems = useMemo<TimelineDisplayItem[]>(
    () => buildTimelineDisplayItems(message),
    [
      message.id,
      message.isThinking,
      message.thinking,
      message.timelineSegments,
      message.timestamp,
      message.terminalExecutions,
    ],
  );
  const hasImageLikeTerminalOutput = useMemo(
    () => timelineItems.some((item) => item.type === 'command' && (item.parsedOutput.images.length > 0 || item.parsedOutput.storagePaths.length > 0)),
    [timelineItems],
  );
  const suppressBinaryContent = useMemo(() => {
    if (isUser || timelineItems.length === 0) {
      return false;
    }
    const normalized = displayContent.trim();
    if (!normalized) {
      return false;
    }
    if (!isLikelyBinaryTextPayload(normalized)) {
      return false;
    }
    if (hasImageLikeTerminalOutput) {
      return true;
    }
    return normalized.length >= 256;
  }, [displayContent, hasImageLikeTerminalOutput, isUser, timelineItems.length]);
  const safeDisplayContent = suppressBinaryContent ? '' : displayContent;

  useEffect(() => {
    setExpandedTimelineItemIds({});
    timelineExpansionOverridesRef.current = {};
  }, [message.id]);

  useEffect(() => {
    if (!isStreaming || message.content.length > 0 || !!message.error) {
      setShowTyping(false);
      return;
    }

    const timer = window.setTimeout(() => setShowTyping(true), 1000);
    return () => window.clearTimeout(timer);
  }, [isStreaming, message.content, message.id, message.error]);

  useEffect(() => {
    if (!showsTasksTag || !sessionId) {
      return;
    }
    let cancelled = false;
    setTasksLoading(true);
    setTasksError(null);
    void fetchSessionTasks(sessionId)
      .then((payload) => {
        if (cancelled) return;
        setTasksContent(payload.content);
        setTasksDraft(payload.content);
        setTasksPlanState(payload.planState);
      })
      .catch((error) => {
        if (cancelled) return;
        setTasksError(error instanceof Error ? error.message : String(error));
      })
      .finally(() => {
        if (!cancelled) {
          setTasksLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, showsTasksTag]);

  const persistTasks = async (
    content: string,
    planState: SessionPlanState,
    nextEditing: boolean,
  ) => {
    if (!sessionId) {
      return;
    }
    setTasksSaving(true);
    setTasksError(null);
    try {
      const payload = await saveSessionTasks(sessionId, {
        content,
        planState,
      });
      setTasksContent(payload.content);
      setTasksDraft(payload.content);
      setTasksPlanState(payload.planState);
      setTasksEditing(nextEditing);
      setSessionPlanState(sessionId, payload.planState);
    } catch (error) {
      setTasksError(error instanceof Error ? error.message : String(error));
    } finally {
      setTasksSaving(false);
    }
  };

  useEffect(() => {
    if (isUser || !onOpenPreview || previewTargets.length === 0) return;
    onOpenPreview(previewTargets[previewTargets.length - 1]);
  }, [isUser, onOpenPreview, previewTargets]);

  useEffect(() => {
    const visibleTimelineItemIds = new Set(timelineItems.map((item) => item.id));
    setExpandedTimelineItemIds((previous) => {
      const nextEntries = Object.entries(previous).filter(([itemId]) =>
        visibleTimelineItemIds.has(itemId),
      );
      const next = Object.fromEntries(nextEntries);
      for (const item of timelineItems) {
        if (timelineExpansionOverridesRef.current[item.id] !== undefined) {
          continue;
        }
        next[item.id] = item.isRunning;
      }
      const previousKeys = Object.keys(previous);
      const nextKeys = Object.keys(next);
      if (previousKeys.length === nextKeys.length) {
        let unchanged = true;
        for (const key of nextKeys) {
          if (previous[key] !== next[key]) {
            unchanged = false;
            break;
          }
        }
        if (unchanged) {
          return previous;
        }
      }
      return next;
    });
    for (const itemId of Object.keys(timelineExpansionOverridesRef.current)) {
      if (!visibleTimelineItemIds.has(itemId)) {
        delete timelineExpansionOverridesRef.current[itemId];
      }
    }
  }, [timelineItems]);

  const nowMs = Date.now();

  return (
    <motion.div
      initial={{ opacity: 0, y: 16, scale: 0.97 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      transition={{ type: 'spring', stiffness: 500, damping: 30 }}
      className={cn('group/message flex w-full gap-1.5 px-2 py-1.5', isUser ? 'flex-row-reverse' : 'flex-row')}>
      <div className={cn('flex flex-col relative', isUser ? 'max-w-[75%] items-end' : 'w-full max-w-[85%] items-start')}>
        <div className={cn(
          'prose prose-sm dark:prose-invert max-w-none break-words relative prose-pre:p-0 prose-pre:m-0 prose-pre:bg-transparent',
          isUser
            ? 'rounded-2xl rounded-br-md bg-primary px-3.5 py-2 text-primary-foreground'
            : 'py-1 text-foreground w-full'
        )}>
          {!isUser && timelineItems.length > 0 && (
            <div className="not-prose mb-1.5 space-y-1">
              {timelineItems.map((item, index) => {
                const isExpanded = expandedTimelineItemIds[item.id] === true;
                const onToggle = () =>
                  setExpandedTimelineItemIds((previous) => {
                    const nextExpanded = !isExpanded;
                    timelineExpansionOverridesRef.current[item.id] = nextExpanded;
                    return { ...previous, [item.id]: nextExpanded };
                  });
                const fallbackStartedAt = Date.parse(message.timestamp);
                return (
                  <TimelineItemRow
                    key={item.id}
                    item={item}
                    isExpanded={isExpanded}
                    onToggle={onToggle}
                    nowMs={nowMs}
                    fallbackStartedAt={fallbackStartedAt}
                    index={index}
                  >
                    {item.type === 'thinking' && <ThinkingTimeline item={item} />}
                    {item.type === 'command' && <CommandTimeline item={item} sessionId={sessionId} />}
                    {item.type === 'websearch' && <WebSearchTimeline item={item} />}
                    {item.type === 'webfetch' && <WebFetchTimeline item={item} />}
                    {item.type === 'browser' && <BrowserTimeline item={item} />}
                  </TimelineItemRow>
                );
              })}
            </div>
          )}

          {message.error && !isUser && (
            <div className="not-prose mt-1 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
              <div>{message.error}</div>
              {onRetry && (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  className="mt-2 h-7 px-2 text-xs"
                  onClick={() => onRetry()}
                >
                  <RotateCcw className="mr-1 h-3 w-3" />
                  Retry
                </Button>
              )}
            </div>
          )}

          {suppressBinaryContent && (
            <div className="not-prose mb-2 rounded-xl border border-cyan-300/30 bg-cyan-500/10 px-3 py-2 text-xs text-cyan-100/95">
              Binary text output hidden. Open the activity timeline to view structured output and images.
            </div>
          )}

          {safeDisplayContent && (
            isUser ? (
              <MemoMarkdown content={safeDisplayContent} components={components} />
            ) : (
              <Suspense fallback={<MemoMarkdown content={safeDisplayContent} components={components} />}>
                <LazyFlowMarkdown content={safeDisplayContent} isStreaming={isStreaming} />
              </Suspense>
            )
          )}

          {!isUser && showsTasksTag && sessionId && (
            <div className="not-prose mt-3 rounded-xl border border-border/70 bg-card/80 px-3 py-3">
              <div className="mb-2 flex items-center justify-between gap-3">
                  <div>
                  <div className="text-sm font-medium text-foreground">tasks.md</div>
                  <div className="text-xs text-muted-foreground">Session task plan exposed via `&lt;tasks /&gt;`</div>
                  <div className="mt-1 text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
                    plan mode: {tasksPlanState}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  {tasksPlanState === "disabled" && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "draft", tasksEditing);
                      }}
                    >
                      Enter Plan
                    </Button>
                  )}
                  {tasksPlanState === "draft" && (
                    <Button
                      type="button"
                      size="sm"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "approved", tasksEditing);
                      }}
                    >
                      Approve
                    </Button>
                  )}
                  {tasksPlanState === "approved" && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      className="h-7 px-2 text-xs"
                      disabled={tasksLoading || tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, "draft", tasksEditing);
                      }}
                    >
                      Reopen Plan
                    </Button>
                  )}
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    className="h-7 px-2 text-xs"
                    disabled={tasksLoading || tasksSaving}
                    onClick={() => {
                      setTasksEditing((value) => !value);
                      setTasksDraft(tasksContent);
                    }}
                  >
                    {tasksEditing ? "Preview" : "Edit"}
                  </Button>
                  {tasksEditing && (
                    <Button
                      type="button"
                      size="sm"
                      className="h-7 px-2 text-xs"
                      disabled={tasksSaving}
                      onClick={() => {
                        void persistTasks(tasksDraft, tasksPlanState, false);
                      }}
                    >
                      {tasksSaving ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : null}
                      Save
                    </Button>
                  )}
                </div>
              </div>
              {tasksError && (
                <div className="mb-2 rounded-md border border-destructive/40 bg-destructive/10 px-2.5 py-2 text-xs text-destructive">
                  {tasksError}
                </div>
              )}
              {tasksLoading ? (
                <div className="text-xs text-muted-foreground">Loading tasks.md…</div>
              ) : tasksEditing ? (
                <textarea
                  value={tasksDraft}
                  onChange={(event) => setTasksDraft(event.target.value)}
                  className="min-h-[220px] w-full rounded-lg border border-border bg-background px-3 py-2 font-mono text-xs text-foreground outline-none focus:ring-2 focus:ring-ring"
                />
              ) : tasksContent.trim() ? (
                <div className="rounded-lg border border-border/60 bg-background/70 px-3 py-2">
                  <MemoMarkdown content={tasksContent} components={components} />
                </div>
              ) : (
                <div className="text-xs text-muted-foreground">tasks.md is empty.</div>
              )}
            </div>
          )}

          {!isUser && previewTargets.length > 0 && (
            <div className="not-prose mt-3 space-y-2">
              {previewTargets.map((target, index) => (
                <div
                  key={`${target.path}:${target.viewer ?? 'default'}:${index}`}
                  className="group flex items-center justify-between rounded-xl border border-border/70 bg-card px-3 py-2"
                >
                  <button
                    type="button"
                    onClick={() => onOpenPreview?.(target)}
                    className="min-w-0 flex-1 text-left"
                  >
                    <div className="truncate text-sm font-medium text-foreground">{target.path}</div>
                    <div className="text-xs text-muted-foreground">
                      {target.viewer === 'slidev' ? 'Slidev preview' : 'Preview'}
                    </div>
                  </button>
                  <a
                    href={target.path.startsWith('http://') || target.path.startsWith('https://') ? target.path : '#'}
                    target="_blank"
                    rel="noreferrer"
                    onClick={(e) => {
                      if (!(target.path.startsWith('http://') || target.path.startsWith('https://'))) {
                        e.preventDefault();
                        onOpenPreview?.(target);
                      }
                    }}
                    className="ml-3 inline-flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
                    title="Open"
                  >
                    <Download className="h-4 w-4" />
                  </a>
                </div>
              ))}
            </div>
          )}
          {!isUser && showTyping && !message.content && !message.error && (
            <div className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.2s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce [animation-delay:-0.1s]" />
              <span className="h-1.5 w-1.5 rounded-full bg-current animate-bounce" />
              <span className="ml-1">typing</span>
            </div>
          )}

        </div>

        <div className={cn('flex items-center gap-2 mt-1 px-1', isUser ? 'flex-row-reverse' : 'flex-row')}>
          <span className="text-[10px] text-muted-foreground">{new Date(message.timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}</span>
          {!isStreaming && (
            <CopyButton
              text={message.content}
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
            />
          )}
          {isUser && canRetry && onRetry && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="h-4 w-4 opacity-0 group-hover/message:opacity-100 transition-opacity text-muted-foreground hover:text-foreground"
              onClick={() => onRetry()}
            >
              <RotateCcw className="h-3 w-3" />
            </Button>
          )}
        </div>

        {message.attachments?.length ? (
          <div className={cn('flex flex-wrap gap-2 mt-2', isUser ? 'justify-end' : 'justify-start')}>
            {message.attachments.map(att => (
              <AttachmentPreview key={att.id} attachment={att}>
                <AttachmentCard attachment={att} variant="message" />
              </AttachmentPreview>
            ))}
          </div>
        ) : null}
      </div>
    </motion.div>
  );
}
