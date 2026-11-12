import { useEffect, useState } from 'react';
import { Play, Square, Copy, ChevronDown, ChevronRight, Cloud, AlertCircle, Loader2, Plus, X } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { useRuntimeStore } from '@/lib/stores/runtime';
import {
  getRemoteAccessStatus,
  startRemoteAccess,
  stopRemoteAccess,
} from '@/lib/api/remoteAccess';
import { addAllowedLogin, getConfig, setFullConfig, withAllowedLogins } from '@/lib/api/config';
import type { RemoteAccessState, RemoteAccessStatusResponse } from '@/lib/types';
import { cn } from '@/lib/utils';

const STATE_TONE: Record<RemoteAccessState, 'neutral' | 'positive' | 'warning' | 'danger'> = {
  disabled: 'neutral',
  starting: 'warning',
  running: 'positive',
  error: 'danger',
};

export function RemoteAccessSection() {
  const { t } = useTranslation();
  const liveStatus = useRuntimeStore((s) => s.remoteAccess);
  const [fallback, setFallback] = useState<RemoteAccessStatusResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [logsOpen, setLogsOpen] = useState(false);
  // The configured list; null until the config is loaded.
  const [logins, setLogins] = useState<string[] | null>(null);
  const [draft, setDraft] = useState('');
  const [savingLogins, setSavingLogins] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [res, cfg] = await Promise.all([getRemoteAccessStatus(), getConfig()]);
        if (!cancelled) {
          setFallback(res);
          setLogins(cfg.fullConfig.remoteAccess.allowedLogins);
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const status = liveStatus ?? fallback;

  const toggle = async () => {
    if (!status) return;
    setBusy(true);
    try {
      if (status.state === 'running' || status.state === 'starting') {
        await stopRemoteAccess();
      } else {
        await startRemoteAccess();
      }
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const saveLogins = async (next: string[]) => {
    setSavingLogins(true);
    try {
      const cfg = await getConfig();
      await setFullConfig(withAllowedLogins(cfg.fullConfig, next));
      setLogins(next);
      setDraft('');
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setSavingLogins(false);
    }
  };

  const submitLogin = () => {
    if (!logins) return;
    const next = addAllowedLogin(logins, draft);
    if (next === logins) {
      setDraft('');
      return;
    }
    void saveLogins(next);
  };

  const copyUrl = async () => {
    if (!status?.publicUrl) return;
    try {
      await navigator.clipboard.writeText(status.publicUrl);
      toast.success('Copied');
    } catch {
      toast.error(t('errors.generic'));
    }
  };

  return (
    <section>
      <SectionHeader
        title={t('settings.remote.title')}
        description={t('settings.remote.description')}
      />

      {loading || !status ? (
        <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : (
        <div className="space-y-4">
          <div className="rounded-xl border border-border bg-card/30 p-5">
            <div className="flex items-start justify-between gap-4">
              <div className="flex items-start gap-3">
                <div
                  className={cn(
                    'mt-0.5 flex size-9 items-center justify-center rounded-lg',
                    status.state === 'running' && 'bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
                    status.state === 'starting' && 'bg-amber-500/10 text-amber-600 dark:text-amber-400',
                    status.state === 'error' && 'bg-destructive/10 text-destructive',
                    status.state === 'disabled' && 'bg-muted text-muted-foreground',
                  )}
                >
                  {status.state === 'starting' ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : status.state === 'error' ? (
                    <AlertCircle className="size-4" />
                  ) : (
                    <Cloud className="size-4" />
                  )}
                </div>
                <div>
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium">{t('settings.remote.status')}</span>
                    <Badge tone={STATE_TONE[status.state]}>
                      {t(`settings.remote.states.${status.state}`)}
                    </Badge>
                  </div>
                  <p className="mt-1 text-xs text-muted-foreground">
                    {status.state === 'running'
                      ? 'Served on your tailnet. Devices signed in to it reach May at the URL below.'
                      : status.state === 'starting'
                      ? 'Asking Tailscale to serve May…'
                      : status.state === 'error'
                      ? 'Tailscale could not serve May.'
                      : 'Serve May over Tailscale to reach it from your other devices.'}
                  </p>
                </div>
              </div>
              <Button
                size="sm"
                variant={status.state === 'running' ? 'outline' : 'default'}
                disabled={busy || status.state === 'starting'}
                onClick={toggle}
              >
                {status.state === 'running' ? (
                  <>
                    <Square className="size-3.5" /> {t('settings.remote.stop')}
                  </>
                ) : (
                  <>
                    <Play className="size-3.5" /> {t('settings.remote.start')}
                  </>
                )}
              </Button>
            </div>

            {status.publicUrl && (
              <div className="mt-4 flex items-center gap-2 rounded-md border border-border bg-background px-3 py-2">
                <div className="min-w-0 flex-1">
                  <div className="text-[11px] uppercase tracking-wide text-muted-foreground">
                    {t('settings.remote.url')}
                  </div>
                  <a
                    href={status.publicUrl}
                    target="_blank"
                    rel="noreferrer"
                    className="block truncate font-mono text-xs text-primary hover:underline"
                  >
                    {status.publicUrl}
                  </a>
                </div>
                <Button size="icon" variant="ghost" onClick={copyUrl} aria-label="Copy URL">
                  <Copy className="size-3.5" />
                </Button>
              </div>
            )}

            {status.lastError && (
              <div className="mt-4 rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-xs">
                <div className="mb-0.5 flex items-center gap-1 font-medium text-destructive">
                  <AlertCircle className="size-3" />
                  {t('settings.remote.lastError')}
                </div>
                <div className="font-mono text-destructive/80">{status.lastError}</div>
              </div>
            )}
          </div>

          <div className="rounded-xl border border-border bg-card/30 p-5">
            <div className="text-sm font-medium">{t('settings.remote.allowedLogins.title')}</div>
            <p
              className={cn(
                'mt-1 text-xs',
                status.accepted?.owner.state === 'unresolved'
                  ? 'text-destructive'
                  : 'text-muted-foreground',
              )}
            >
              {!status.accepted
                ? t('settings.remote.allowedLogins.pending')
                : status.accepted.owner.state === 'resolved'
                  ? t('settings.remote.allowedLogins.owner', { login: status.accepted.owner.login })
                  : t('settings.remote.allowedLogins.unresolved', {
                      reason: status.accepted.owner.reason,
                    })}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              {t('settings.remote.allowedLogins.hint')}
            </p>

            {logins === null ? (
              <div className="mt-3 flex items-center gap-2 text-xs text-muted-foreground">
                <Loader2 className="size-3.5 animate-spin" /> {t('common.loading')}
              </div>
            ) : (
              <>
                {logins.length === 0 ? (
                  <p className="mt-3 text-xs text-muted-foreground">
                    {t('settings.remote.allowedLogins.empty')}
                  </p>
                ) : (
                  <ul className="mt-3 space-y-1.5">
                    {logins.map((login) => (
                      <li
                        key={login}
                        className="flex items-center gap-2 rounded-md border border-border bg-background px-3 py-1.5"
                      >
                        <span className="min-w-0 flex-1 truncate font-mono text-xs">{login}</span>
                        <Button
                          size="icon"
                          variant="ghost"
                          disabled={savingLogins}
                          aria-label={t('settings.remote.allowedLogins.remove', { login })}
                          onClick={() => void saveLogins(logins.filter((l) => l !== login))}
                        >
                          <X className="size-3.5" />
                        </Button>
                      </li>
                    ))}
                  </ul>
                )}
                <div className="mt-3 flex items-center gap-2">
                  <Input
                    value={draft}
                    disabled={savingLogins}
                    placeholder={t('settings.remote.allowedLogins.placeholder')}
                    onChange={(e) => setDraft(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') {
                        e.preventDefault();
                        submitLogin();
                      }
                    }}
                  />
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={savingLogins || draft.trim() === ''}
                    onClick={submitLogin}
                  >
                    {savingLogins ? (
                      <>
                        <Loader2 className="size-3.5 animate-spin" />{' '}
                        {t('settings.remote.allowedLogins.saving')}
                      </>
                    ) : (
                      <>
                        <Plus className="size-3.5" /> {t('settings.remote.allowedLogins.add')}
                      </>
                    )}
                  </Button>
                </div>
              </>
            )}
          </div>

          {status.recentLogs.length > 0 && (
            <div>
              <button
                type="button"
                onClick={() => setLogsOpen((v) => !v)}
                className="flex items-center gap-1 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground"
              >
                {logsOpen ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
                {t('settings.remote.logs')}
                <span className="text-muted-foreground/60">({status.recentLogs.length})</span>
              </button>
              {logsOpen && (
                <pre className="mt-2 max-h-64 overflow-auto rounded-md border border-border bg-card/30 p-3 font-mono text-[11px] leading-relaxed text-muted-foreground">
                  {status.recentLogs.join('\n')}
                </pre>
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
