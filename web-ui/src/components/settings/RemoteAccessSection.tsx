import { useEffect } from 'react';
import { Shield } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useStore } from '@/lib/store';
import type { ConfigSectionProps } from './constants';

export function RemoteAccessSection({ configDraft, setDraft }: ConfigSectionProps) {
  const {
    remoteAccessStatus,
    refreshRemoteAccessStatus,
    startRemoteAccess,
    stopRemoteAccess,
  } = useStore();

  useEffect(() => {
    void refreshRemoteAccessStatus();
  }, [refreshRemoteAccessStatus]);

  if (!configDraft) {
    return <div className="text-sm text-muted-foreground">Loading config...</div>;
  }

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase tracking-wide text-muted-foreground">
        <Shield className="h-4 w-4" /> Remote Access
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <div className="space-y-1 md:col-span-2">
          <Label className="text-sm">Enable remote access</Label>
          <div className="flex items-center gap-3 pt-2">
            <input
              type="checkbox"
              checked={configDraft.remoteAccess.enabled}
              onChange={(event) =>
                setDraft((prev) => ({
                  ...prev,
                  remoteAccess: {
                    ...prev.remoteAccess,
                    enabled: event.target.checked,
                  },
                }))
              }
            />
            <span className="text-xs text-muted-foreground">
              Runs a named Cloudflare tunnel through `cloudflared`. Quick tunnels are intentionally unsupported because the app depends on SSE.
            </span>
          </div>
        </div>

        <div className="space-y-1">
          <Label>Cloudflared Binary</Label>
          <Input
            value={configDraft.remoteAccess.cloudflaredBinary}
            onChange={(event) =>
              setDraft((prev) => ({
                ...prev,
                remoteAccess: {
                  ...prev.remoteAccess,
                  cloudflaredBinary: event.target.value,
                },
              }))
            }
          />
        </div>

        <div className="space-y-1">
          <Label>Public URL</Label>
          <Input
            value={configDraft.remoteAccess.publicUrl ?? ''}
            onChange={(event) =>
              setDraft((prev) => ({
                ...prev,
                remoteAccess: {
                  ...prev.remoteAccess,
                  publicUrl: event.target.value,
                },
              }))
            }
            placeholder="https://may.example.com"
          />
        </div>

        <div className="space-y-1 md:col-span-2">
          <Label>Tunnel Token</Label>
          <Input
            type="password"
            value={configDraft.remoteAccess.tunnelToken ?? ''}
            onChange={(event) =>
              setDraft((prev) => ({
                ...prev,
                remoteAccess: {
                  ...prev.remoteAccess,
                  tunnelToken: event.target.value,
                },
              }))
            }
            placeholder="Cloudflare tunnel token"
          />
        </div>

        <div className="space-y-1 md:col-span-2">
          <Label className="text-sm">Allow local-environment exposure</Label>
          <div className="flex items-center gap-3 pt-2">
            <input
              type="checkbox"
              checked={configDraft.remoteAccess.allowLocalEnvironment}
              onChange={(event) =>
                setDraft((prev) => ({
                  ...prev,
                  remoteAccess: {
                    ...prev.remoteAccess,
                    allowLocalEnvironment: event.target.checked,
                  },
                }))
              }
            />
            <span className="text-xs text-muted-foreground">
              Disabled by default because local profiles target the user’s real machine.
            </span>
          </div>
        </div>
      </div>

      <div className="rounded-xl border border-border/70 bg-card/70 p-4">
        <div className="flex items-center justify-between gap-4">
          <div className="space-y-1">
            <div className="text-sm font-medium text-foreground">
              Status: {remoteAccessStatus?.state ?? 'disabled'}
            </div>
            <div className="text-xs text-muted-foreground">
              {remoteAccessStatus?.publicUrl ?? configDraft.remoteAccess.publicUrl ?? 'No public URL configured'}
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" onClick={() => void refreshRemoteAccessStatus()}>
              Refresh
            </Button>
            <Button onClick={() => void startRemoteAccess()}>
              Start
            </Button>
            <Button variant="outline" onClick={() => void stopRemoteAccess()}>
              Stop
            </Button>
          </div>
        </div>
        {remoteAccessStatus?.lastError ? (
          <div className="mt-3 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {remoteAccessStatus.lastError}
          </div>
        ) : null}
        {remoteAccessStatus?.recentLogs?.length ? (
          <pre className="mt-3 max-h-64 overflow-auto rounded-lg border border-border/60 bg-background/80 p-3 text-[11px] leading-relaxed text-muted-foreground">
            {remoteAccessStatus.recentLogs.join('\n')}
          </pre>
        ) : null}
      </div>
    </section>
  );
}
