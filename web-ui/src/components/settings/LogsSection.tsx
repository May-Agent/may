import { useEffect, useState } from 'react';
import { BookOpen } from 'lucide-react';
import * as api from '@/lib/api';
import type {
  NetworkAccessLogEntry,
  NotificationMailboxEntry,
  RequestLogCacheSummary,
  RequestLogEntry,
} from '@/lib/api';

export function LogsSection() {
  const [requestLogs, setRequestLogs] = useState<RequestLogEntry[]>([]);
  const [cacheSummary, setCacheSummary] = useState<RequestLogCacheSummary | null>(null);
  const [mailbox, setMailbox] = useState<NotificationMailboxEntry[]>([]);
  const [networkLogs, setNetworkLogs] = useState<NetworkAccessLogEntry[]>([]);
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
        const payload = await api.fetchRequestLogs();
        const mailboxPayload = await api.fetchNotificationMailbox();
        const networkPayload = await api.fetchNetworkAccessLogs();
        if (!cancelled) {
          setRequestLogs(payload.logs);
          setCacheSummary(payload.cacheSummary);
          setMailbox(mailboxPayload.items);
          setNetworkLogs(networkPayload.items);
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
        setCacheSummary(payload.cacheSummary);
        setRequestLogsError(null);
        clearDisconnectError();
      },
      onNotificationMailbox: (payload) => {
        if (cancelled) return;
        setMailbox(payload.items);
        clearDisconnectError();
      },
      onNetworkAccessLogs: (payload) => {
        if (cancelled) return;
        setNetworkLogs(payload.items);
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
      {cacheSummary && (
        <div className="rounded border px-3 py-2 text-xs text-muted-foreground">
          cache summary: assistant_messages={cacheSummary.assistantMessageCount} cache_hit_messages={cacheSummary.cacheHitMessageCount} prompt_tokens={cacheSummary.promptTokensTotal} cache_read={cacheSummary.cacheReadTokensTotal} cache_write={cacheSummary.cacheWriteTokensTotal} hit_rate={cacheSummary.cacheReadHitRate != null ? `${(cacheSummary.cacheReadHitRate * 100).toFixed(1)}%` : '-'}
        </div>
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
            <div>
              tokens: prompt={log.promptTokens ?? '-'} completion={log.completionTokens ?? '-'} total={log.totalTokens ?? '-'} cache_read={log.cacheReadTokens ?? '-'} cache_write={log.cacheWriteTokens ?? '-'} window={log.contextWindow ?? '-'}
            </div>
            <div>latency: {log.elapsedMs}ms</div>
            {log.error && <div className="text-red-500 break-all">{log.error}</div>}
          </div>
        ))}
      </div>
      <div className="space-y-2">
        <div className="text-sm font-medium uppercase text-muted-foreground tracking-wide">
          Notification Mailbox
        </div>
        <div className="rounded border divide-y max-h-[320px] overflow-auto">
          {mailbox.length === 0 && (
            <div className="px-3 py-3 text-sm text-muted-foreground">No notifications yet.</div>
          )}
          {mailbox.map((item) => (
            <div key={item.id} className="px-3 py-2 text-xs space-y-1">
              <div className="flex items-center justify-between gap-3">
                <span className="font-medium">{item.title}</span>
                <span className="text-muted-foreground">{item.status}</span>
              </div>
              <div>{item.body}</div>
              <div className="text-muted-foreground">
                created={item.createdAt} sent={item.sentCount} callback={item.hasCallback ? 'yes' : 'no'}
              </div>
              {item.sessionId && (
                <div className="text-muted-foreground">session={item.sessionId}</div>
              )}
              {item.lastError && (
                <div className="text-destructive break-all">{item.lastError}</div>
              )}
            </div>
          ))}
        </div>
      </div>
      <div className="space-y-2">
        <div className="text-sm font-medium uppercase text-muted-foreground tracking-wide">
          Network Audit
        </div>
        <div className="rounded border divide-y max-h-[320px] overflow-auto">
          {networkLogs.length === 0 && (
            <div className="px-3 py-3 text-sm text-muted-foreground">No network audit events yet.</div>
          )}
          {networkLogs.map((item) => (
            <div key={item.id} className="px-3 py-2 text-xs space-y-1">
              <div className="flex items-center justify-between gap-3">
                <span className="font-mono">{item.timestamp}</span>
                <span className={item.allowed ? 'text-green-500' : 'text-red-500'}>
                  {item.allowed ? 'allowed' : 'blocked'}
                </span>
              </div>
              <div className="break-all">
                {item.domain}:{item.port} · source={item.source} · mode={item.mode}
              </div>
              <div className="text-muted-foreground">
                session={item.sessionId ?? '-'} profile={item.profileId ?? '-'}
              </div>
              {item.reason && (
                <div className="text-muted-foreground break-all">{item.reason}</div>
              )}
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
