import { useEffect, useMemo, useState } from 'react';
import { Activity, CheckCircle2, XCircle } from 'lucide-react';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { getRequestLogs } from '@/lib/api/logs';
import type { RequestLogsResponse } from '@/lib/types';
import { cn, useFormatRelativeTime } from '@/lib/utils';

function formatTokens(n: number | null): string {
  if (n == null) return '—';
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

export function ActivityLogsSection() {
  const { t } = useTranslation();
  const live = useRuntimeStore((s) => s.requestLogs);
  const [fallback, setFallback] = useState<RequestLogsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const formatRelative = useFormatRelativeTime();

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await getRequestLogs();
        if (!cancelled) setFallback(res);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const data = live ?? fallback;
  const cacheRate = data?.cacheSummary.cacheReadHitRate;
  const totalPromptTokens = data?.cacheSummary.promptTokensTotal ?? 0;
  const totalCacheTokens = data?.cacheSummary.cacheReadTokensTotal ?? 0;
  const assistantCount = data?.cacheSummary.assistantMessageCount ?? 0;

  const avgElapsed = useMemo(() => {
    if (!data || data.logs.length === 0) return 0;
    const sum = data.logs.reduce((acc, l) => acc + l.elapsedMs, 0);
    return sum / data.logs.length;
  }, [data]);

  return (
    <section>
      <SectionHeader
        title={t('settings.logs.title')}
        description={t('settings.logs.description')}
      />

      {loading || !data ? (
        <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : data.logs.length === 0 ? (
        <div className="flex flex-col items-center justify-center gap-2 py-14 text-sm text-muted-foreground">
          <Activity className="size-6 opacity-40" />
          {t('settings.logs.empty')}
        </div>
      ) : (
        <div className="space-y-4">
          <div className="grid grid-cols-3 gap-3">
            <Stat
              label={t('settings.logs.cacheHitRate')}
              value={cacheRate != null ? `${(cacheRate * 100).toFixed(0)}%` : '—'}
              hint={`${totalCacheTokens.toLocaleString()} / ${totalPromptTokens.toLocaleString()} tok`}
              accent={cacheRate != null && cacheRate > 0.5 ? 'positive' : 'neutral'}
            />
            <Stat
              label={t('settings.logs.tokens')}
              value={formatTokens(totalPromptTokens + totalCacheTokens)}
              hint={`across ${assistantCount} ${assistantCount === 1 ? 'request' : 'requests'}`}
              accent="neutral"
            />
            <Stat
              label={`${t('settings.logs.elapsed')} · avg`}
              value={formatDuration(Math.round(avgElapsed))}
              hint={`${data.logs.length} recent`}
              accent="neutral"
            />
          </div>

          <ul className="divide-y divide-border rounded-lg border border-border bg-card/20">
            {data.logs.map((log, idx) => {
              const cacheRatio = log.promptTokens && log.cacheReadTokens != null
                ? log.cacheReadTokens / Math.max(1, log.promptTokens)
                : null;
              return (
                <li
                  key={`${log.sessionId}-${log.timestamp}-${idx}`}
                  className="flex items-center gap-3 px-4 py-2.5 text-sm"
                >
                  {log.ok ? (
                    <CheckCircle2 className="size-3.5 shrink-0 text-emerald-500" />
                  ) : (
                    <XCircle className="size-3.5 shrink-0 text-destructive" />
                  )}
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="truncate font-mono text-[12px]">{log.model}</span>
                      {cacheRatio != null && (
                        <CacheBar ratio={cacheRatio} />
                      )}
                    </div>
                    {log.error && (
                      <div className="mt-0.5 truncate font-mono text-[11px] text-destructive">
                        {log.error}
                      </div>
                    )}
                  </div>
                  <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
                    {formatTokens(log.totalTokens)}
                  </span>
                  <span className="shrink-0 font-mono text-[11px] text-muted-foreground">
                    {formatDuration(log.elapsedMs)}
                  </span>
                  <span className="shrink-0 text-[11px] text-muted-foreground">
                    {formatRelative(log.timestamp)}
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </section>
  );
}

interface StatProps {
  label: string;
  value: string;
  hint: string;
  accent: 'positive' | 'neutral';
}

function Stat({ label, value, hint, accent }: StatProps) {
  return (
    <div className="rounded-lg border border-border bg-card/20 p-3">
      <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div
        className={cn(
          'mt-1 text-xl font-semibold tabular-nums',
          accent === 'positive' && 'text-emerald-600 dark:text-emerald-400',
        )}
      >
        {value}
      </div>
      <div className="mt-0.5 text-[11px] text-muted-foreground">{hint}</div>
    </div>
  );
}

function CacheBar({ ratio }: { ratio: number }) {
  const pct = Math.min(1, Math.max(0, ratio));
  return (
    <div
      className="h-1 w-12 overflow-hidden rounded-full bg-muted"
      title={`${(pct * 100).toFixed(0)}% cached`}
    >
      <div className="h-full bg-emerald-500" style={{ width: `${pct * 100}%` }} />
    </div>
  );
}

