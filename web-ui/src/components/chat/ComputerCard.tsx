import { memo } from 'react';
import { Monitor } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import type { TurnToolState } from '@/lib/stream/reducer';
import { type BrowserActivity, browserActivity, hostOf } from '@/lib/browser/activity';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import { useActiveProfile } from '@/lib/profile/active';
import { profileInitial } from '@/lib/profile/env';
import { artifactPreviewUrl } from '@/lib/api/artifacts';
import { cn } from '@/lib/utils';

type T = ReturnType<typeof useTranslation>['t'];

function pageName(a: BrowserActivity): string {
  return a.pageTitle ?? (a.pageUrl ? hostOf(a.pageUrl) : a.url ? hostOf(a.url) : '');
}

/** What the agent is doing right now, e.g. "Taking a screenshot". */
function doingLabel(a: BrowserActivity, t: T): string {
  switch (a.action) {
    case 'open':
      return t('computer.actionOpen');
    case 'close':
      return t('computer.actionClose');
    case 'status':
      return t('computer.actionStatus');
    case 'connect':
      return t('computer.actionConnect');
    case 'read':
      if (a.screenshot) return t('computer.actionScreenshot');
      return a.url ? t('computer.actionRead', { host: hostOf(a.url) }) : t('computer.actionOther');
    case 'other':
      return t('computer.actionOther');
  }
}

/** What the agent did, e.g. "Read Pricing - Example". */
function didLabel(a: BrowserActivity, t: T): string {
  switch (a.action) {
    case 'open':
      return t('computer.didOpen');
    case 'close':
      return t('computer.didClose');
    case 'status':
      return t('computer.didStatus');
    case 'connect':
      return t('computer.didConnect');
    case 'read': {
      const page = pageName(a);
      return page ? t('computer.didRead', { page }) : t('computer.didOther');
    }
    case 'other':
      return t('computer.didOther');
  }
}

/** The agent's avatar: its profile initial on an ink disc. */
function AgentAvatar() {
  const { profile } = useActiveProfile();
  const { t } = useTranslation();
  return (
    <span
      className="flex size-5 shrink-0 items-center justify-center rounded-full bg-foreground text-[10px] font-semibold text-background"
      aria-hidden
    >
      {profileInitial(profile?.name ?? t('appName'))}
    </span>
  );
}

function Screenshots({ items, sessionId }: { items: BrowserActivity[]; sessionId: string }) {
  const { t } = useTranslation();
  const shots = items.filter((a) => a.screenshotPath);
  if (shots.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2">
      {shots.map((a) => {
        const src = artifactPreviewUrl(sessionId, a.screenshotPath as string);
        return (
          <a
            key={a.screenshotPath}
            href={src}
            target="_blank"
            rel="noreferrer"
            className="block w-60 max-w-full overflow-hidden rounded-md border border-border bg-muted"
          >
            <img
              src={src}
              alt={t('computer.screenshotOf', { page: pageName(a) || t('computer.title') })}
              className="aspect-[16/10] w-full object-cover object-top"
              loading="lazy"
            />
          </a>
        );
      })}
    </div>
  );
}

/**
 * One stretch of browser use in the chat. While the agent acts it is a
 * single muted line ("Taking a screenshot"); once it finishes it becomes a
 * quiet card with what was done, any screenshots, and Open computer.
 */
export const ComputerCard = memo(function ComputerCard({
  tools,
  sessionId,
}: {
  tools: TurnToolState[];
  sessionId: string;
}) {
  const { t } = useTranslation();
  const openViewer = useBrowserViewStore((s) => s.openViewer);
  const items = tools.map(browserActivity);
  const current = items[items.length - 1];
  if (!current) return null;

  if (current.status === 'running') {
    return (
      <div className="space-y-2">
        <div role="status" className="flex items-center gap-2 text-[13px] text-muted-foreground">
          <AgentAvatar />
          <span className="shimmer-text">{doingLabel(current, t)}</span>
        </div>
        <Screenshots items={items} sessionId={sessionId} />
      </div>
    );
  }

  const failed = items.find((a) => a.status === 'failed');
  const steps = items.map((a) => didLabel(a, t));
  const summary = steps[steps.length - 1] ?? '';
  const status = failed ? t('computer.failed') : t('computer.done');

  return (
    <section
      aria-label={t('computer.title')}
      className="overflow-hidden rounded-lg border border-border bg-card"
    >
      <header className="flex items-center gap-2 border-b border-border px-3 py-2 text-[13px]">
        <Monitor className="size-4 shrink-0 text-muted-foreground" strokeWidth={1.75} aria-hidden />
        <span className="font-medium">{t('computer.title')}</span>
        <span className="min-w-0 truncate text-muted-foreground" title={summary}>
          {status} · {summary}
        </span>
      </header>
      <div className="flex flex-col gap-3 px-3 py-2.5 sm:flex-row sm:items-end">
        <div className="min-w-0 flex-1 space-y-2.5">
          <p className="text-[13px] leading-5">
            <span className={cn('mr-1.5 font-medium', failed && 'text-destructive')}>{status}</span>
            <span className="text-muted-foreground">
              {steps.join('. ')}
              {failed?.error ? `. ${failed.error}` : '.'}
            </span>
          </p>
          <Screenshots items={items} sessionId={sessionId} />
        </div>
        <Button size="sm" variant="secondary" className="shrink-0 self-end" onClick={openViewer}>
          <Monitor className="size-3.5" strokeWidth={1.75} aria-hidden />
          {t('computer.open')}
        </Button>
      </div>
    </section>
  );
});
