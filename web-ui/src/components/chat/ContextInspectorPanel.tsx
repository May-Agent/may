import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { X, Copy, ChevronDown, ChevronRight as ChevronRightIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  fetchSessionContextDebug,
  type SessionContextDebugSnapshot,
} from "@/lib/api";
import {
  buildContextRoleTokenBreakdown,
  estimateMessageTokens,
  getContextRoleBarClass,
  normalizeContextRole,
  type ContextRoleTokenBreakdown,
} from "./chatUtils";

interface ContextInspectorPanelProps {
  currentSessionId: string | null;
  promptTokens?: number;
  contextUsedTokens?: number;
  contextWindow?: number;
  contextUsagePercent: number | null;
  onClose: () => void;
}

function formatNumber(value: number | undefined | null): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return "--";
  return value.toLocaleString();
}

export function ContextInspectorPanel({
  currentSessionId,
  promptTokens,
  contextUsedTokens,
  contextWindow,
  contextUsagePercent,
  onClose,
}: ContextInspectorPanelProps) {
  const [snapshot, setSnapshot] = useState<SessionContextDebugSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [expandedMessages, setExpandedMessages] = useState<Set<number>>(
    new Set(),
  );
  const requestVersionRef = useRef(0);
  const currentSessionIdRef = useRef<string | null>(currentSessionId);

  useEffect(() => {
    currentSessionIdRef.current = currentSessionId;
  }, [currentSessionId]);

  const loadSnapshot = useCallback(async () => {
    if (!currentSessionId) return;
    const requestSessionId = currentSessionId;
    const requestVersion = requestVersionRef.current + 1;
    requestVersionRef.current = requestVersion;
    setLoading(true);
    setError(null);
    try {
      const payload = await fetchSessionContextDebug(requestSessionId);
      if (
        requestVersionRef.current !== requestVersion ||
        currentSessionIdRef.current !== requestSessionId
      ) {
        return;
      }
      setSnapshot(payload.snapshot);
    } catch (err) {
      if (
        requestVersionRef.current !== requestVersion ||
        currentSessionIdRef.current !== requestSessionId
      ) {
        return;
      }
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      if (
        requestVersionRef.current === requestVersion &&
        currentSessionIdRef.current === requestSessionId
      ) {
        setLoading(false);
      }
    }
  }, [currentSessionId]);

  useEffect(() => {
    void loadSnapshot();
    setExpandedMessages(new Set());
  }, [loadSnapshot]);

  const snapshotForCurrentSession =
    snapshot && snapshot.sessionId === currentSessionId ? snapshot : null;
  const effectiveWindow = snapshotForCurrentSession?.windowSnapshot;
  const displayedMessages =
    effectiveWindow?.assembledMessages ??
    snapshotForCurrentSession?.assembledMessages ??
    [];
  const roleTokenBreakdown: ContextRoleTokenBreakdown | null =
    snapshotForCurrentSession
      ? buildContextRoleTokenBreakdown(snapshotForCurrentSession, promptTokens)
      : null;
  const sortedBuckets = useMemo(
    () =>
      roleTokenBreakdown
        ? [...roleTokenBreakdown.buckets].sort((a, b) => b.tokens - a.tokens)
        : null,
    [roleTokenBreakdown],
  );

  const barPercent = Math.min(100, Math.max(0, contextUsagePercent ?? 0));

  const toggleMessage = (index: number) => {
    setExpandedMessages((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  const copyMessage = async (content: string) => {
    try {
      await navigator.clipboard.writeText(content);
    } catch {
      // Clipboard can fail in restricted contexts; silently ignore.
    }
  };

  return (
    <aside className="flex h-full w-full min-w-0 flex-col border-l bg-background">
      <div className="flex shrink-0 items-center justify-between border-b px-4 py-3">
        <div className="text-base font-semibold">Context</div>
        <Button variant="ghost" size="icon" className="h-8 w-8" onClick={onClose}>
          <X className="h-4 w-4" />
        </Button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        {loading && !snapshotForCurrentSession ? (
          <div className="text-sm text-muted-foreground">Loading context snapshot…</div>
        ) : error ? (
          <div className="text-sm text-destructive">
            Failed to load snapshot: {error}
          </div>
        ) : !snapshotForCurrentSession ? (
          <div className="text-sm text-muted-foreground">
            No context snapshot for this session yet.
          </div>
        ) : (
          <div className="space-y-6">
            <section>
              <div className="flex items-baseline justify-between gap-2 text-sm">
                <span className="tabular-nums">
                  <span className="font-medium text-foreground">
                    {formatNumber(contextUsedTokens)}
                  </span>
                  <span className="text-muted-foreground">
                    {" "}/ {formatNumber(contextWindow)} tokens
                  </span>
                </span>
                <span className="text-xs text-muted-foreground tabular-nums">
                  {contextUsagePercent != null ? `${contextUsagePercent}%` : "--"} used
                </span>
              </div>
              <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted/40">
                <div
                  className="h-full rounded-full bg-foreground/80 transition-[width] duration-300"
                  style={{ width: `${barPercent}%` }}
                />
              </div>
            </section>

            {sortedBuckets && sortedBuckets.length > 0 && (
              <section>
                <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
                  Breakdown
                </div>
                <div className="space-y-1.5 text-sm">
                  {sortedBuckets.map((bucket) => {
                    const total = Math.max(1, roleTokenBreakdown?.total ?? 0);
                    const pct = Math.round((bucket.tokens / total) * 100);
                    return (
                      <div
                        key={bucket.role}
                        className="flex items-center justify-between gap-3"
                      >
                        <span className="flex min-w-0 items-center gap-2">
                          <span
                            aria-hidden
                            className={`inline-block h-2 w-2 shrink-0 rounded-full ${getContextRoleBarClass(bucket.role)}`}
                          />
                          <span className="truncate">{bucket.role}</span>
                        </span>
                        <span className="shrink-0 tabular-nums text-muted-foreground">
                          {formatNumber(bucket.tokens)} tok · {bucket.messages} msg
                          <span className="ml-2 text-foreground/80">{pct}%</span>
                        </span>
                      </div>
                    );
                  })}
                </div>
              </section>
            )}

            <section>
              <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
                Session
              </div>
              <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
                <dt className="text-muted-foreground">Model</dt>
                <dd className="font-mono text-xs">
                  {snapshotForCurrentSession.model} ({snapshotForCurrentSession.providerId})
                </dd>
                <dt className="text-muted-foreground">Window</dt>
                <dd className="font-mono text-xs tabular-nums">
                  {formatNumber(effectiveWindow?.metrics.effectiveContextWindow) ?? "--"} tokens
                </dd>
                <dt className="text-muted-foreground">Messages</dt>
                <dd className="font-mono text-xs tabular-nums">
                  {displayedMessages.length} assembled
                </dd>
                {effectiveWindow && (
                  <>
                    <dt className="text-muted-foreground">Usage</dt>
                    <dd className="font-mono text-xs tabular-nums">
                      {(effectiveWindow.metrics.usageFraction * 100).toFixed(1)}%
                    </dd>
                  </>
                )}
              </dl>
            </section>

            {displayedMessages.length > 0 && (
              <section>
                <div className="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
                  Messages ({displayedMessages.length})
                </div>
                <div className="space-y-1.5">
                  {displayedMessages.map((message, index) => {
                    const role = normalizeContextRole(message.role);
                    const isExpanded = expandedMessages.has(index);
                    const tokens = estimateMessageTokens(message);
                    return (
                      <div
                        key={`${role}-${index}`}
                        className="rounded-lg border border-border/50 bg-muted/10"
                      >
                        <button
                          type="button"
                          onClick={() => toggleMessage(index)}
                          className="flex w-full items-center gap-2 px-3 py-2 text-left"
                          aria-expanded={isExpanded}
                        >
                          {isExpanded ? (
                            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                          ) : (
                            <ChevronRightIcon className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                          )}
                          <span className="flex min-w-0 items-center gap-2">
                            <span
                              aria-hidden
                              className={`inline-block h-2 w-2 shrink-0 rounded-full ${getContextRoleBarClass(role)}`}
                            />
                            <span className="truncate text-sm">{role}</span>
                          </span>
                          <span className="ml-auto shrink-0 font-mono text-[11px] tabular-nums text-muted-foreground">
                            {formatNumber(tokens)} tok
                          </span>
                        </button>
                        <div
                          className={cn(
                            "px-3 pb-2",
                            isExpanded ? "" : "line-clamp-2 opacity-60",
                          )}
                        >
                          <pre className="whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed text-foreground">
                            {message.content}
                          </pre>
                          {isExpanded && (
                            <div className="mt-2 flex justify-end">
                              <Button
                                variant="ghost"
                                size="sm"
                                className="h-7 gap-1 px-2 text-[11px]"
                                onClick={() => void copyMessage(message.content)}
                              >
                                <Copy className="h-3 w-3" />
                                Copy
                              </Button>
                            </div>
                          )}
                        </div>
                      </div>
                    );
                  })}
                </div>
              </section>
            )}
          </div>
        )}
      </div>
    </aside>
  );
}
