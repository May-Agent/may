import { useEffect, useMemo, useState } from 'react';
import { Shield, ShieldOff, Globe } from 'lucide-react';
import { SectionHeader } from './SectionHeader';
import { Segmented } from '@/components/ui/segmented';
import { Badge } from '@/components/ui/badge';
import { useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { getNetworkAccessLogs } from '@/lib/api/networkAccess';
import type {
  NetworkAccessLogEntryResponse,
  NetworkAccessLogsResponse,
  NetworkAccessSource,
} from '@/lib/types';
import { cn, useFormatRelativeTime } from '@/lib/utils';

type Filter = 'all' | 'allowed' | 'denied';

const SOURCE_LABEL: Record<NetworkAccessSource, string> = {
  chat: 'Chat',
  exec: 'Exec',
  task_local: 'Task · local',
  task_validation_local: 'Validation · local',
  task_remote: 'Task · remote',
  task_container: 'Task · container',
  task_validation_container: 'Validation · container',
};

export function NetworkSection() {
  const { t } = useTranslation();
  const live = useRuntimeStore((s) => s.networkAccess);
  const [fallback, setFallback] = useState<NetworkAccessLogsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [filter, setFilter] = useState<Filter>('all');
  const formatRelative = useFormatRelativeTime();

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await getNetworkAccessLogs();
        if (!cancelled) setFallback(res);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const logs: NetworkAccessLogEntryResponse[] = useMemo(() => {
    const data = live ?? fallback;
    return data ? data.items : [];
  }, [live, fallback]);

  const filtered = useMemo(() => {
    if (filter === 'all') return logs;
    if (filter === 'allowed') return logs.filter((l) => l.allowed);
    return logs.filter((l) => !l.allowed);
  }, [logs, filter]);

  const counts = useMemo(() => {
    let allowed = 0;
    let denied = 0;
    for (const l of logs) {
      if (l.allowed) allowed += 1;
      else denied += 1;
    }
    return { allowed, denied };
  }, [logs]);

  return (
    <section>
      <SectionHeader
        title={t('settings.network.title')}
        description={t('settings.network.description')}
        action={
          logs.length > 0 ? (
            <Segmented<Filter>
              size="sm"
              value={filter}
              onValueChange={setFilter}
              options={[
                { value: 'all', label: `All · ${logs.length}` },
                {
                  value: 'allowed',
                  label: (
                    <span className="inline-flex items-center gap-1.5">
                      <Shield className="size-3" />
                      {t('settings.network.allowed')} · {counts.allowed}
                    </span>
                  ),
                },
                {
                  value: 'denied',
                  label: (
                    <span className="inline-flex items-center gap-1.5">
                      <ShieldOff className="size-3" />
                      {t('settings.network.denied')} · {counts.denied}
                    </span>
                  ),
                },
              ]}
            />
          ) : undefined
        }
      />

      {loading ? (
        <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : filtered.length === 0 ? (
        <EmptyState label={logs.length === 0 ? t('settings.network.empty') : 'No entries match this filter'} />
      ) : (
        <ul className="divide-y divide-border rounded-lg border border-border bg-card/20">
          {filtered.map((entry) => (
            <li
              key={entry.id}
              className={cn(
                'flex items-center gap-3 px-4 py-2.5 text-sm',
                !entry.allowed && 'bg-amber-500/[0.03]',
              )}
            >
              <span
                aria-hidden
                className={cn(
                  'inline-block size-2 shrink-0 rounded-full',
                  entry.allowed ? 'bg-emerald-500' : 'bg-amber-500',
                )}
              />
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <span className="truncate font-mono text-[13px]">{entry.domain}</span>
                  {entry.port !== 443 && entry.port !== 80 && (
                    <span className="text-muted-foreground/70 text-[11px]">:{entry.port}</span>
                  )}
                </div>
                {entry.reason && (
                  <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{entry.reason}</div>
                )}
              </div>
              <Badge tone="neutral" className="shrink-0">
                {SOURCE_LABEL[entry.source] ?? entry.source}
              </Badge>
              <span className="shrink-0 text-[11px] text-muted-foreground">
                {formatRelative(entry.timestamp)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function EmptyState({ label }: { label: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 py-12 text-sm text-muted-foreground">
      <Globe className="size-6 opacity-40" />
      <span>{label}</span>
    </div>
  );
}
