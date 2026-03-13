import { useState, useCallback, useRef, useEffect } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Eye, FileText } from "lucide-react";
import {
  fetchSessionContextDebug,
  type SessionContextDebugSnapshot,
} from "@/lib/api";
import {
  buildContextRoleTokenBreakdown,
  estimateMessageTokens,
  getContextRoleBadgeClass,
  getContextRoleBarClass,
  normalizeContextRole,
  type ContextDebugSnapshotLoadResult,
  type ContextRoleTokenBreakdown,
} from "./chatUtils";

interface ContextInspectorProps {
  currentSessionId: string | null;
  promptTokens?: number;
  contextUsedTokens?: number;
  contextWindow?: number;
  contextUsagePercent: number | null;
  contextUsageTitle: string;
}

export function ContextInspector({
  currentSessionId,
  promptTokens,
  contextUsedTokens,
  contextWindow,
  contextUsagePercent,
  contextUsageTitle,
}: ContextInspectorProps) {
  const [snapshot, setSnapshot] = useState<SessionContextDebugSnapshot | null>(null);
  const [rawPayload, setRawPayload] = useState<{
    sessionId: string;
    rawJson: string;
  } | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isUsagePanelOpen, setIsUsagePanelOpen] = useState(false);
  const [isInspectorOpen, setIsInspectorOpen] = useState(false);

  const requestVersionRef = useRef(0);
  const currentSessionIdRef = useRef<string | null>(currentSessionId);

  useEffect(() => {
    currentSessionIdRef.current = currentSessionId;
    requestVersionRef.current += 1;
    setSnapshot(null);
    setRawPayload(null);
    setError(null);
    setLoading(false);
    setIsUsagePanelOpen(false);
    setIsInspectorOpen(false);
  }, [currentSessionId]);

  const snapshotForCurrentSession =
    snapshot && snapshot.sessionId === currentSessionId ? snapshot : null;
  const rawJsonForCurrentSession =
    rawPayload && rawPayload.sessionId === currentSessionId
      ? rawPayload.rawJson
      : null;
  const roleTokenBreakdown: ContextRoleTokenBreakdown | null =
    snapshotForCurrentSession
      ? buildContextRoleTokenBreakdown(snapshotForCurrentSession, promptTokens)
      : null;
  const effectiveWindowSnapshot = snapshotForCurrentSession?.windowSnapshot;
  const displayedMessages =
    effectiveWindowSnapshot?.assembledMessages ??
    snapshotForCurrentSession?.assembledMessages ??
    [];
  const messageCount = displayedMessages.length;

  const loadSnapshot = useCallback(async (): Promise<ContextDebugSnapshotLoadResult> => {
    if (!currentSessionId) {
      return { ok: false, error: "No active session selected." };
    }
    if (
      snapshot &&
      snapshot.sessionId === currentSessionId &&
      rawPayload &&
      rawPayload.sessionId === currentSessionId
    ) {
      return {
        ok: true,
        payload: {
          snapshot,
          rawJson: rawPayload.rawJson,
        },
      };
    }
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
        return {
          ok: false,
          error: `Context debug request superseded for session ${requestSessionId}.`,
        };
      }
      setSnapshot(payload.snapshot);
      setRawPayload({
        sessionId: payload.snapshot.sessionId,
        rawJson: payload.rawJson,
      });
      return { ok: true, payload };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (
        requestVersionRef.current !== requestVersion ||
        currentSessionIdRef.current !== requestSessionId
      ) {
        return {
          ok: false,
          error: `Context debug request superseded for session ${requestSessionId}.`,
        };
      }
      setError(message);
      return { ok: false, error: message };
    } finally {
      if (
        requestVersionRef.current === requestVersion &&
        currentSessionIdRef.current === requestSessionId
      ) {
        setLoading(false);
      }
    }
  }, [currentSessionId, rawPayload, snapshot]);

  const openInspector = useCallback(async () => {
    const result = await loadSnapshot();
    if (!result.ok) {
      return;
    }
    setRawPayload({
      sessionId: result.payload.snapshot.sessionId,
      rawJson: result.payload.rawJson,
    });
    setSnapshot(result.payload.snapshot);
    setIsInspectorOpen(true);
  }, [loadSnapshot]);

  return (
    <>
      <div
        className="relative"
        onMouseEnter={() => {
          setIsUsagePanelOpen(true);
          if (currentSessionId) {
            void loadSnapshot();
          }
        }}
        onMouseLeave={() => setIsUsagePanelOpen(false)}
      >
        <button
          type="button"
          className="inline-flex h-8 items-center gap-1 rounded-lg px-2 text-xs text-muted-foreground hover:text-foreground"
          title={contextUsageTitle}
          onFocus={() => {
            setIsUsagePanelOpen(true);
            if (currentSessionId) {
              void loadSnapshot();
            }
          }}
          onBlur={() => setIsUsagePanelOpen(false)}
        >
          <Eye className="h-4 w-4" />
          <span>{contextUsagePercent != null ? `${contextUsagePercent}%` : "--"}</span>
        </button>
        {isUsagePanelOpen && (
          <div className="absolute bottom-full right-0 z-50 mb-2 w-72 rounded-xl border bg-popover p-3 text-xs text-popover-foreground shadow-xl">
            <div className="text-sm font-medium">Context usage</div>
            <div className="mt-1 text-muted-foreground">
              {contextUsedTokens ?? "-"} / {contextWindow ?? "-"} tokens ({contextUsagePercent ?? "-"}%)
            </div>
            <div className="mt-3 space-y-1">
              {loading ? (
                <div className="text-muted-foreground">Loading context distribution...</div>
              ) : error ? (
                <div className="text-destructive">
                  Failed to load distribution: {error}
                </div>
              ) : roleTokenBreakdown ? (
                <>
                  {roleTokenBreakdown.buckets.map((bucket) => (
                    <div key={bucket.role} className="flex items-center justify-between gap-2">
                      <span
                        className={`inline-flex items-center rounded-md border px-1.5 py-0.5 text-[10px] font-medium ${getContextRoleBadgeClass(bucket.role)}`}
                      >
                        {bucket.role}
                      </span>
                      <span className="text-right">
                        {bucket.tokens} tok ({bucket.messages} msg)
                      </span>
                    </div>
                  ))}
                  <div className="text-muted-foreground">
                    Prompt total: {roleTokenBreakdown.total} tokens
                  </div>
                </>
              ) : (
                <div className="text-muted-foreground">No context snapshot yet.</div>
              )}
            </div>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="mt-3 h-7 px-2 text-xs"
              onClick={() => void openInspector()}
            >
              <FileText className="mr-1 h-3.5 w-3.5" />
              Open context inspector
            </Button>
          </div>
        )}
      </div>

      {currentSessionId && (
        <Button
          onClick={() => void openInspector()}
          size="icon"
          variant="ghost"
          className="h-8 w-8 rounded-lg text-muted-foreground hover:text-foreground"
          title="Open context inspector"
        >
          <FileText className="h-4 w-4" />
        </Button>
      )}

      <Dialog open={isInspectorOpen} onOpenChange={setIsInspectorOpen}>
        <DialogContent className="h-[88vh] w-[96vw] max-w-6xl overflow-hidden p-0">
          <div className="flex h-full min-h-0 flex-col bg-background">
            <DialogHeader className="border-b px-5 py-4 text-left">
              <DialogTitle>Context Inspector</DialogTitle>
              <DialogDescription>
                Exact context payload plus role-level message/token breakdown and current assembled window state.
              </DialogDescription>
            </DialogHeader>
            {loading ? (
              <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
                Loading context snapshot...
              </div>
            ) : error ? (
              <div className="flex h-full items-center justify-center px-8 text-sm text-destructive">
                Failed to load context snapshot: {error}
              </div>
            ) : !snapshotForCurrentSession || !rawJsonForCurrentSession ? (
              <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
                No context snapshot for this session yet.
              </div>
            ) : (
              <div className="flex min-h-0 flex-1 flex-col">
                <div className="grid grid-cols-2 gap-2 border-b px-4 py-3 text-xs md:grid-cols-4">
                  <div className="rounded-lg border bg-muted/30 px-3 py-2">
                    <div className="text-[11px] uppercase text-muted-foreground">Session</div>
                    <div className="truncate font-medium">{snapshotForCurrentSession.sessionId}</div>
                  </div>
                  <div className="rounded-lg border bg-muted/30 px-3 py-2">
                    <div className="text-[11px] uppercase text-muted-foreground">Model</div>
                    <div className="truncate font-medium">
                      {snapshotForCurrentSession.model} ({snapshotForCurrentSession.providerId})
                    </div>
                  </div>
                  <div className="rounded-lg border bg-muted/30 px-3 py-2">
                    <div className="text-[11px] uppercase text-muted-foreground">Messages</div>
                    <div className="font-medium">{messageCount}</div>
                  </div>
                  <div className="rounded-lg border bg-muted/30 px-3 py-2">
                    <div className="text-[11px] uppercase text-muted-foreground">Window phase</div>
                    <div className="font-medium">
                      {effectiveWindowSnapshot?.phase ?? "request_preview"}
                    </div>
                  </div>
                </div>
                <div className="grid grid-cols-1 gap-3 border-b px-4 py-3 text-xs md:grid-cols-2">
                  <div className="rounded-lg border p-3">
                    <div className="mb-2 text-sm font-medium">Role breakdown</div>
                    {roleTokenBreakdown ? (
                      <div className="space-y-3">
                        {roleTokenBreakdown.buckets.map((bucket) => {
                          const total = Math.max(1, roleTokenBreakdown.total);
                          const tokenPercent = Math.round((bucket.tokens / total) * 100);
                          return (
                            <div key={bucket.role}>
                              <div className="flex items-center justify-between gap-3 text-[11px]">
                                <span
                                  className={`inline-flex items-center rounded-md border px-1.5 py-0.5 font-medium ${getContextRoleBadgeClass(bucket.role)}`}
                                >
                                  {bucket.label}
                                </span>
                                <span className="font-mono text-foreground">
                                  {bucket.messages} msg / {bucket.tokens} tok / {tokenPercent}%
                                </span>
                              </div>
                              <div className="mt-1.5 h-1.5 overflow-hidden rounded-full bg-muted/35">
                                <div
                                  className={`h-full rounded-full ${getContextRoleBarClass(bucket.role)}`}
                                  style={{ width: `${tokenPercent}%` }}
                                />
                              </div>
                            </div>
                          );
                        })}
                      </div>
                    ) : (
                      <div className="text-muted-foreground">No breakdown available.</div>
                    )}
                  </div>
                  <div className="rounded-lg border p-3">
                    <div className="mb-2 text-sm font-medium">Window metrics</div>
                    {effectiveWindowSnapshot ? (
                      <div className="space-y-1 text-[11px]">
                        <div>usage={effectiveWindowSnapshot.metrics.usageFraction.toFixed(3)}</div>
                        <div>effective window={effectiveWindowSnapshot.metrics.effectiveContextWindow}</div>
                        <div>selected model window={effectiveWindowSnapshot.metrics.selectedModelContextWindow ?? "-"}</div>
                        <div>fast model window={effectiveWindowSnapshot.metrics.fastModelContextWindow ?? "-"}</div>
                        <div>system blocks={effectiveWindowSnapshot.metrics.systemBlockCount} reminders={effectiveWindowSnapshot.metrics.reminderCount}</div>
                        <div>recent messages={effectiveWindowSnapshot.metrics.recentMessageCount} system messages={effectiveWindowSnapshot.metrics.recentSystemMessageCount} handoff={effectiveWindowSnapshot.metrics.hasHandoff ? "yes" : "no"}</div>
                      </div>
                    ) : (
                      <pre className="max-h-28 overflow-auto whitespace-pre-wrap rounded border bg-muted/30 p-2 font-mono text-[11px] leading-4">
                        {snapshotForCurrentSession.prompt}
                      </pre>
                    )}
                  </div>
                </div>
                <div className="grid min-h-0 flex-1 grid-cols-1 gap-3 p-4 md:grid-cols-2">
                  <div className="flex min-h-0 flex-col rounded-xl border">
                    <div className="border-b px-3 py-2 text-sm font-medium">
                      Assembled messages ({messageCount})
                    </div>
                    <div className="min-h-0 flex-1 space-y-2 overflow-auto p-3">
                      {displayedMessages.map((message, index) => {
                        const role = normalizeContextRole(message.role);
                        return (
                          <div key={`${role}-${index}`} className="rounded-lg border bg-muted/10 p-2.5">
                            <div className="mb-2 flex items-center justify-between gap-2">
                              <span
                                className={`inline-flex items-center rounded-md border px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide ${getContextRoleBadgeClass(role)}`}
                              >
                                {role}
                              </span>
                              <span className="font-mono text-[11px] text-muted-foreground">
                                {estimateMessageTokens(message)} tok
                              </span>
                            </div>
                            <pre className="whitespace-pre-wrap break-words font-mono text-[11px] leading-4 text-foreground">
                              {message.content}
                            </pre>
                          </div>
                        );
                      })}
                    </div>
                  </div>
                  <div className="flex min-h-0 flex-col rounded-xl border">
                    <div className="border-b px-3 py-2 text-sm font-medium">Raw context payload</div>
                    <pre className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap break-words bg-muted/20 p-3 font-mono text-[11px] leading-4 text-foreground">
                      {rawJsonForCurrentSession}
                    </pre>
                  </div>
                </div>
              </div>
            )}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
