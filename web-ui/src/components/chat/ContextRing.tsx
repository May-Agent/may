import { useEffect, useState } from 'react';
import { Gauge, Flame } from 'lucide-react';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
import { getSessionContextDebug } from '@/lib/api/logs';
import { useTranslation } from '@/lib/i18n';
import type { SessionContextDebugResponse } from '@/lib/types';
import { cn } from '@/lib/utils';

const POLL_MS = 15_000;

function formatTokens(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

function tone(fraction: number): { stroke: string; text: string } {
  if (fraction >= 0.9) return { stroke: 'stroke-destructive', text: 'text-destructive' };
  if (fraction >= 0.7) return { stroke: 'stroke-amber-500', text: 'text-amber-600 dark:text-amber-400' };
  return { stroke: 'stroke-primary', text: 'text-foreground' };
}

export function ContextRing({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  const [data, setData] = useState<SessionContextDebugResponse | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const res = await getSessionContextDebug(sessionId);
        if (!cancelled) setData(res);
      } catch {
        // ignore — context debug is best-effort
      }
    };
    void load();
    const handle = window.setInterval(load, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(handle);
    };
  }, [sessionId]);

  const metrics = data?.windowSnapshot?.metrics;
  const fraction = metrics?.usageFraction ?? 0;
  const pct = Math.min(100, Math.max(0, Math.round(fraction * 100)));
  const totalWindow = metrics?.contextWindow ?? null;
  const { stroke, text } = tone(fraction);

  const radius = 7;
  const circumference = 2 * Math.PI * radius;
  const dashOffset = circumference * (1 - Math.min(1, fraction));

  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          className={cn(
            'group inline-flex items-center gap-1.5 rounded-full border border-border bg-card/60 px-2 py-0.5 text-[11px] font-medium tabular-nums transition-colors hover:bg-accent',
            text,
          )}
          aria-label={t('chat.context')}
        >
          <svg width="16" height="16" viewBox="0 0 18 18" aria-hidden>
            <circle cx="9" cy="9" r={radius} fill="none" className="stroke-muted" strokeWidth="2" />
            <circle
              cx="9"
              cy="9"
              r={radius}
              fill="none"
              className={cn(stroke, 'transition-[stroke-dashoffset] duration-500')}
              strokeWidth="2"
              strokeLinecap="round"
              strokeDasharray={circumference}
              strokeDashoffset={dashOffset}
              transform="rotate(-90 9 9)"
            />
          </svg>
          {fraction >= 0.9 ? <Flame className="size-3" /> : null}
          <span>{pct}%</span>
        </button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-72 space-y-3">
        <div className="flex items-center gap-2">
          <Gauge className="size-3.5 text-muted-foreground" />
          <div className="text-sm font-medium">{t('chat.context')}</div>
        </div>
        {!metrics ? (
          <div className="text-xs text-muted-foreground">
            Context metrics will appear after May’s first reply.
          </div>
        ) : (
          <>
            <div className="text-xs text-muted-foreground">
              {t('chat.contextUsage', {
                used: pct.toString(),
                total: totalWindow ? formatTokens(totalWindow) : '—',
              })}
            </div>
            <div className="grid grid-cols-2 gap-2">
              <Stat label="Window" value={formatTokens(metrics.contextWindow)} />
              <Stat label="Estimated" value={formatTokens(metrics.estimatedTokens)} />
              <Stat label="Recent messages" value={metrics.recentMessageCount.toString()} />
              <Stat label="System blocks" value={metrics.systemBlockCount.toString()} />
              <Stat
                label="Handoff"
                value={metrics.hasHandoff ? 'active' : '—'}
              />
            </div>
          </>
        )}
      </PopoverContent>
    </Popover>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border border-border bg-card/40 px-2 py-1.5">
      <div className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="mt-0.5 font-mono text-xs">{value}</div>
    </div>
  );
}
