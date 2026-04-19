import { useState, useCallback, useRef, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Eye, FileText } from "lucide-react";
import {
  fetchSessionContextDebug,
  type SessionContextDebugSnapshot,
} from "@/lib/api";
import {
  buildContextRoleTokenBreakdown,
  getContextRoleBarClass,
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
  /** Called when the user asks to open the full context panel on the right. */
  onOpenPanel: () => void;
}

function formatNumber(value: number | undefined): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return "--";
  return value.toLocaleString();
}

export function ContextInspector({
  currentSessionId,
  promptTokens,
  contextUsedTokens,
  contextWindow,
  contextUsagePercent,
  contextUsageTitle,
  onOpenPanel,
}: ContextInspectorProps) {
  const [snapshot, setSnapshot] = useState<SessionContextDebugSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isUsagePanelOpen, setIsUsagePanelOpen] = useState(false);

  const requestVersionRef = useRef(0);
  const currentSessionIdRef = useRef<string | null>(currentSessionId);

  useEffect(() => {
    currentSessionIdRef.current = currentSessionId;
    requestVersionRef.current += 1;
    setSnapshot(null);
    setError(null);
    setLoading(false);
    setIsUsagePanelOpen(false);
  }, [currentSessionId]);

  const snapshotForCurrentSession =
    snapshot && snapshot.sessionId === currentSessionId ? snapshot : null;
  const roleTokenBreakdown: ContextRoleTokenBreakdown | null =
    snapshotForCurrentSession
      ? buildContextRoleTokenBreakdown(snapshotForCurrentSession, promptTokens)
      : null;
  // Larger roles on top so the user sees the dominant cost first.
  const sortedBuckets = roleTokenBreakdown
    ? [...roleTokenBreakdown.buckets].sort((a, b) => b.tokens - a.tokens)
    : null;

  const loadSnapshot = useCallback(async (): Promise<ContextDebugSnapshotLoadResult> => {
    if (!currentSessionId) {
      return { ok: false, error: "No active session selected." };
    }
    if (snapshot && snapshot.sessionId === currentSessionId) {
      return {
        ok: true,
        payload: { snapshot, rawJson: "" },
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
  }, [currentSessionId, snapshot]);

  const barPercent = Math.min(100, Math.max(0, contextUsagePercent ?? 0));

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
          <div className="absolute bottom-full right-0 z-50 mb-2 w-72 rounded-xl border bg-popover p-4 text-xs text-popover-foreground shadow-xl">
            <div className="flex items-baseline justify-between gap-2">
              <div className="text-sm font-medium">Context</div>
              <div className="tabular-nums text-muted-foreground">
                {formatNumber(contextUsedTokens)} / {formatNumber(contextWindow)}
              </div>
            </div>
            <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-muted/40">
              <div
                className="h-full rounded-full bg-foreground/80 transition-[width] duration-300"
                style={{ width: `${barPercent}%` }}
              />
            </div>
            <div className="mt-1 text-right text-[11px] text-muted-foreground tabular-nums">
              {contextUsagePercent != null ? `${contextUsagePercent}%` : "--"} used
            </div>
            <div className="mt-3 space-y-1.5">
              {loading && !sortedBuckets ? (
                <div className="text-muted-foreground">Loading distribution…</div>
              ) : error ? (
                <div className="text-destructive">
                  Failed to load distribution: {error}
                </div>
              ) : sortedBuckets && sortedBuckets.length > 0 ? (
                sortedBuckets.map((bucket) => (
                  <div
                    key={bucket.role}
                    className="flex items-center justify-between gap-2"
                  >
                    <span className="flex min-w-0 items-center gap-2">
                      <span
                        aria-hidden
                        className={`inline-block h-2 w-2 shrink-0 rounded-full ${getContextRoleBarClass(bucket.role)}`}
                      />
                      <span className="truncate text-foreground">
                        {bucket.role}
                      </span>
                    </span>
                    <span className="shrink-0 tabular-nums text-muted-foreground">
                      {formatNumber(bucket.tokens)} · {bucket.messages} msg
                    </span>
                  </div>
                ))
              ) : (
                <div className="text-muted-foreground">No context snapshot yet.</div>
              )}
            </div>
            <button
              type="button"
              className="mt-3 text-xs text-muted-foreground hover:text-foreground"
              onClick={onOpenPanel}
            >
              Inspect context →
            </button>
          </div>
        )}
      </div>

      {currentSessionId && (
        <Button
          onClick={onOpenPanel}
          size="icon"
          variant="ghost"
          className="h-8 w-8 rounded-lg text-muted-foreground hover:text-foreground"
          title="Open context inspector"
        >
          <FileText className="h-4 w-4" />
        </Button>
      )}
    </>
  );
}
