import { useEffect, useState } from 'react';
import { BookOpen } from 'lucide-react';
import * as api from '@/lib/api';
import type { RequestLogEntry } from '@/lib/api';

export function LogsSection() {
  const [requestLogs, setRequestLogs] = useState<RequestLogEntry[]>([]);
  const [requestLogsError, setRequestLogsError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let disconnectTimer: number | null = null;

    const clearDisconnectError = () => {
      if (disconnectTimer != null) {
        window.clearTimeout(disconnectTimer);
        disconnectTimer = null;
      }
      setRequestLogsError((prev) =>
        prev === 'Logs event stream disconnected; reconnecting...' ? null : prev,
      );
    };

    const loadLogs = async () => {
      try {
        const logs = await api.fetchRequestLogs();
        if (!cancelled) {
          setRequestLogs(logs);
          setRequestLogsError(null);
          clearDisconnectError();
        }
      } catch (error) {
        if (!cancelled) {
          setRequestLogsError(error instanceof Error ? error.message : String(error));
        }
      }
    };

    void loadLogs();
    const unsubscribe = api.subscribeUiEvents({
      onOpen: clearDisconnectError,
      onHeartbeat: clearDisconnectError,
      onRequestLogs: (payload) => {
        if (cancelled) return;
        setRequestLogs(payload.logs);
        setRequestLogsError(null);
        clearDisconnectError();
      },
      onError: () => {
        if (disconnectTimer != null) return;
        disconnectTimer = window.setTimeout(() => {
          disconnectTimer = null;
          if (!cancelled) {
            setRequestLogsError('Logs event stream disconnected; reconnecting...');
          }
        }, 1200);
      },
    });

    return () => {
      cancelled = true;
      if (disconnectTimer != null) {
        window.clearTimeout(disconnectTimer);
      }
      unsubscribe();
    };
  }, []);

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <BookOpen className="h-4 w-4" /> Request Logs
      </div>
      {requestLogsError && (
        <div className="text-xs text-destructive">{requestLogsError}</div>
      )}
      <div className="rounded border divide-y max-h-[560px] overflow-auto">
        {requestLogs.length === 0 && (
          <div className="px-3 py-3 text-sm text-muted-foreground">No request logs yet.</div>
        )}
        {requestLogs.map((log, idx) => (
          <div key={`${log.timestamp}-${idx}`} className="px-3 py-2 text-xs space-y-1">
            <div className="flex items-center justify-between gap-3">
              <span className="font-mono">{log.timestamp}</span>
              <span className={log.ok ? 'text-green-500' : 'text-red-500'}>{log.ok ? 'ok' : 'error'}</span>
            </div>
            <div className="text-muted-foreground">session={log.sessionId}</div>
            <div>model=<span className="font-mono">{log.model}</span> · provider=<span className="font-mono">{log.providerId}</span></div>
            <div>tokens: prompt={log.promptTokens ?? '-'} completion={log.completionTokens ?? '-'} total={log.totalTokens ?? '-'} window={log.contextWindow ?? '-'}</div>
            <div>latency: {log.elapsedMs}ms</div>
            {log.error && <div className="text-red-500 break-all">{log.error}</div>}
          </div>
        ))}
      </div>
    </section>
  );
}
