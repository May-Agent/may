import { useCallback, useEffect, useRef, useState } from 'react';
import { Loader2, GitBranch, Plus, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import {
  configureForgeAuth,
  getForgeAuthStatus,
  logoutForge,
  pollForgeOauth,
  startForgeOauth,
  type ForgeAuthMode,
  type ForgeHostStatus,
} from '@/lib/api/forgeAuth';
import { cn } from '@/lib/utils';

const MODES: { id: ForgeAuthMode; label: string }[] = [
  { id: 'token', label: 'Token' },
  { id: 'oauth', label: 'OAuth' },
  { id: 'cli', label: 'CLI' },
];

interface DeviceFlow {
  userCode: string;
  verificationUri: string;
}

export function HostCard({
  host,
  kind,
  configured,
  mode,
  client_id,
  authenticated,
  onChanged,
}: ForgeHostStatus & { onChanged: () => void }) {
  const { t } = useTranslation();
  const [picked, setPicked] = useState<ForgeAuthMode>(mode ?? 'token');
  const [token, setToken] = useState('');
  const [clientId, setClientId] = useState(client_id ?? '');
  const [busy, setBusy] = useState(false);
  const [device, setDevice] = useState<DeviceFlow | null>(null);
  const cancelled = useRef(false);
  useEffect(() => () => void (cancelled.current = true), []);

  const fail = (prefix: string, error: unknown) =>
    toast.error(`${prefix}: ${error instanceof Error ? error.message : String(error)}`);

  const save = async () => {
    setBusy(true);
    try {
      if (picked === 'token') {
        if (!token.trim()) {
          toast.error(t('forge.tokenRequired'));
          return;
        }
        await configureForgeAuth({ host, mode: 'token', token: token.trim() });
        setToken('');
      } else if (picked === 'cli') {
        await configureForgeAuth({ host, mode: 'cli' });
      }
      onChanged();
    } catch (error) {
      fail(t('forge.saveFailed'), error);
    } finally {
      setBusy(false);
    }
  };

  const oauth = async () => {
    setBusy(true);
    try {
      const auth = await startForgeOauth({
        host,
        client_id: clientId.trim() || undefined,
      });
      setDevice({
        userCode: auth.user_code,
        verificationUri: auth.verification_uri_complete ?? auth.verification_uri,
      });
      let interval = Math.max(1, auth.interval_secs) * 1000;
      // Poll until the daemon answers `authorized` or this component unmounts.
      for (;;) {
        await new Promise((resolve) => setTimeout(resolve, interval));
        if (cancelled.current) return;
        const poll = await pollForgeOauth(host, auth.device_code);
        if (poll.status === 'authorized') {
          if (!cancelled.current) {
            setDevice(null);
            toast.success(t('forge.connected', { host }));
            onChanged();
          }
          return;
        }
        interval = Math.max(1, poll.retry_after_secs) * 1000;
      }
    } catch (error) {
      if (!cancelled.current) fail(t('forge.oauthFailed'), error);
    } finally {
      if (!cancelled.current) setBusy(false);
    }
  };

  const disconnect = async () => {
    setBusy(true);
    try {
      await logoutForge(host);
      toast.success(t('forge.disconnected', { host }));
      onChanged();
    } catch (error) {
      fail(t('forge.disconnectFailed'), error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rounded-md border border-border p-3">
      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">{host}</span>
        {kind && <Badge tone="neutral">{kind}</Badge>}
        <Badge
          tone={authenticated ? 'positive' : 'neutral'}
          className={cn(!authenticated && 'opacity-60')}
        >
          {authenticated ? t('forge.connectedBadge') : t('forge.unauthenticated')}
        </Badge>
        {configured && (
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto h-7 px-2"
            disabled={busy}
            onClick={() => void disconnect()}
            aria-label={t('forge.disconnect', { host })}
          >
            <Trash2 className="size-3.5" />
          </Button>
        )}
      </div>
      {device ? (
        <div className="mt-3 space-y-1 text-xs">
          <p>
            {t('forge.devicePrompt')}{' '}
            <a className="underline" href={device.verificationUri} target="_blank" rel="noreferrer">
              {device.verificationUri}
            </a>
          </p>
          <p className="font-mono text-sm">{device.userCode}</p>
          <p className="text-muted-foreground">{t('forge.waiting')}</p>
        </div>
      ) : (
        <div className="mt-3 space-y-2">
          <div className="flex gap-1">
            {MODES.map(({ id, label }) => (
              <Button
                key={id}
                size="sm"
                variant={picked === id ? 'default' : 'outline'}
                className="h-7 px-2 text-xs"
                onClick={() => setPicked(id)}
              >
                {label}
              </Button>
            ))}
          </div>
          {picked === 'token' && (
            <Input
              type="password"
              placeholder={t('forge.tokenPlaceholder')}
              value={token}
              onChange={(e) => setToken(e.target.value)}
            />
          )}
          {picked === 'oauth' && (
            <Input
              placeholder={t('forge.clientIdPlaceholder')}
              value={clientId}
              onChange={(e) => setClientId(e.target.value)}
            />
          )}
          {picked === 'oauth' ? (
            <Button size="sm" disabled={busy} onClick={() => void oauth()}>
              {busy && <Loader2 className="size-3.5 animate-spin" />}
              {t('forge.startOauth')}
            </Button>
          ) : (
            <Button size="sm" disabled={busy} onClick={() => void save()}>
              {busy && <Loader2 className="size-3.5 animate-spin" />}
              {t('forge.save')}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}

export function ForgeSection() {
  const { t } = useTranslation();
  const [hosts, setHosts] = useState<ForgeHostStatus[] | null>(null);
  const [newHost, setNewHost] = useState('');
  const [newKind, setNewKind] = useState<'github' | 'gitlab'>('github');
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const res = await getForgeAuthStatus();
      setHosts(res.hosts);
    } catch (error) {
      toast.error(
        `Forge status: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await getForgeAuthStatus();
        if (!cancelled) setHosts(res.hosts);
      } catch (error) {
        if (!cancelled) {
          toast.error(
            `Forge status: ${error instanceof Error ? error.message : String(error)}`,
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const addHost = async () => {
    const host = newHost.trim();
    if (!host) return;
    setBusy(true);
    try {
      await configureForgeAuth({ host, kind: newKind, mode: 'cli' });
      setNewHost('');
      await refresh();
    } catch (error) {
      toast.error(
        `${t('forge.saveFailed')}: ${error instanceof Error ? error.message : String(error)}`,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <SectionHeader
        title={t('forge.title')}
        description={t('forge.subtitle')}
      />
      {hosts === null ? (
        <Loader2 className="size-4 animate-spin" />
      ) : (
        <div className="space-y-3">
          {hosts.map((status) => (
            <HostCard key={status.host} {...status} onChanged={() => void refresh()} />
          ))}
          <div className="flex items-center gap-2">
            <Plus className="size-4 text-muted-foreground" />
            <Input
              className="w-48"
              placeholder="gitlab.corp.com"
              value={newHost}
              onChange={(e) => setNewHost(e.target.value)}
            />
            <select
              className="h-8 rounded-md border border-border bg-transparent px-2 text-xs"
              value={newKind}
              onChange={(e) => setNewKind(e.target.value as 'github' | 'gitlab')}
              aria-label={t('forge.kind')}
            >
              <option value="github">GitHub</option>
              <option value="gitlab">GitLab</option>
            </select>
            <Button
              size="sm"
              variant="outline"
              disabled={busy || !newHost.trim()}
              onClick={() => void addHost()}
            >
              <GitBranch className="size-3.5" />
              {t('forge.addHost')}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
