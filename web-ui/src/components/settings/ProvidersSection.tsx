import { createElement, useCallback, useEffect, useState } from 'react';
import { Boxes, Plus, Trash2, ExternalLink } from 'lucide-react';
import * as api from '@/lib/api';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import type { UiProviderConfig, ProviderModelsItem } from '@/lib/api';
import {
  PROVIDER_TYPES,
  PROVIDER_TYPE_LABEL,
  providerIcon,
  type ConfigSectionProps,
} from './constants';

export function ProvidersSection({ configDraft, setDraft }: ConfigSectionProps) {
  const [newProviderType, setNewProviderType] = useState<UiProviderConfig['providerType']>('openai');
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  const [providerModels, setProviderModels] = useState<Record<string, api.ModelDetailsItem[]>>({});
  const [copilotAuth, setCopilotAuth] = useState<{
    authenticated: boolean;
    loading: boolean;
    polling: boolean;
    deviceCode?: string;
    userCode?: string;
    verificationUri?: string;
    message?: string;
  }>({ authenticated: false, loading: false, polling: false });

  const fetchProviderModelsWithRetry = useCallback(async (): Promise<ProviderModelsItem[]> => {
    const maxAttempts = 4;
    for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
      const rows = await api.fetchModelsByProvider() as ProviderModelsItem[];
      if (rows.length > 0 || attempt === maxAttempts - 1) {
        return rows;
      }
      await new Promise<void>((resolve) => {
        window.setTimeout(resolve, 200);
      });
    }
    return [];
  }, []);

  useEffect(() => {
    if (!configDraft?.providers.length) {
      setSelectedProviderId(null);
      return;
    }
    if (!selectedProviderId || !configDraft.providers.some((p) => p.id === selectedProviderId)) {
      setSelectedProviderId(configDraft.providers[0]?.id ?? null);
    }
  }, [configDraft, selectedProviderId]);

  useEffect(() => {
    let cancelled = false;
    const loadStatus = async () => {
      try {
        const status = await api.fetchCopilotStatus();
        if (!cancelled) {
          setCopilotAuth((s) => ({ ...s, authenticated: status.authenticated }));
        }
      } catch (error) {
        if (!cancelled) {
          setCopilotAuth((s) => ({
            ...s,
            message: error instanceof Error ? error.message : String(error),
          }));
        }
      }
    };
    void loadStatus();
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    if (!copilotAuth.polling || !copilotAuth.deviceCode) return;
    let cancelled = false;
    const timer = window.setInterval(async () => {
      if (cancelled) return;
      try {
        const res = await api.pollCopilotAuth(copilotAuth.deviceCode!);
        if (cancelled) return;
        if (res.status === 'pending') return;
        if (res.status === 'success') {
          setCopilotAuth((s) => ({
            ...s,
            polling: false,
            authenticated: true,
            message: 'Copilot connected',
          }));
          return;
        }
        setCopilotAuth((s) => ({
          ...s,
          polling: false,
          message: res.message ?? `Copilot auth ${res.status}`,
        }));
      } catch (e) {
        if (!cancelled) {
          setCopilotAuth((s) => ({ ...s, polling: false, message: (e as Error).message }));
        }
      }
    }, 2500);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [copilotAuth.polling, copilotAuth.deviceCode]);

  useEffect(() => {
    let cancelled = false;
    const loadProviderModels = async () => {
      try {
        const rows = await fetchProviderModelsWithRetry();
        if (cancelled) return;
        const grouped: Record<string, api.ModelDetailsItem[]> = {};
        for (const row of rows as ProviderModelsItem[]) {
          grouped[row.providerId] =
            row.details && row.details.length > 0
              ? row.details
              : row.models.map((id) => ({ id, nickname: id, abilities: [] }));
        }
        setProviderModels(grouped);
      } catch {
        // silently ignore; models will show "No models fetched yet"
      }
    };
    void loadProviderModels();
    return () => { cancelled = true; };
  }, [configDraft?.providers.length, fetchProviderModelsWithRetry]);

  const startCopilotOAuth = async () => {
    setCopilotAuth((s) => ({ ...s, loading: true, message: undefined }));
    try {
      const start = await api.startCopilotAuth();
      setCopilotAuth((s) => ({
        ...s,
        loading: false,
        polling: true,
        deviceCode: start.deviceCode,
        userCode: start.userCode,
        verificationUri: start.verificationUri,
      }));
      window.open(start.verificationUri, '_blank', 'noopener,noreferrer');
    } catch (e) {
      setCopilotAuth((s) => ({ ...s, loading: false, message: (e as Error).message }));
    }
  };

  if (!configDraft) {
    return <div className="text-sm text-muted-foreground">Loading provider config...</div>;
  }

  const selectedProvider = configDraft.providers.find((p) => p.id === selectedProviderId) ?? null;
  const selectedProviderModels = selectedProvider ? providerModels[selectedProvider.id] ?? [] : [];

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <Boxes className="h-4 w-4" /> Providers
      </div>

      <div className="grid gap-4 md:grid-cols-[280px_1fr]">
        <div className="rounded border bg-muted/20 p-2 space-y-2">
          <div className="flex items-center gap-2">
            <Select value={newProviderType} onValueChange={(v) => setNewProviderType(v as UiProviderConfig['providerType'])}>
              <SelectTrigger className="h-8"><SelectValue /></SelectTrigger>
              <SelectContent>
                {PROVIDER_TYPES.map((p) => (
                  <SelectItem key={p} value={p}>{p}</SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              size="icon"
              variant="outline"
              className="h-8 w-8"
              onClick={() => {
                const baseId = newProviderType;
                let id: string = baseId;
                let n = 2;
                while (configDraft.providers.some((p) => p.id === id)) {
                  id = `${baseId}-${n++}`;
                }
                setSelectedProviderId(id);
                setDraft((prev) => ({
                  ...prev,
                  providers: [
                    ...prev.providers,
                    {
                      id,
                      providerType: newProviderType,
                      apiKey: null,
                      baseUrl: null,
                      displayName: null,
                      enabled: true,
                    },
                  ].sort((a, b) => a.id.localeCompare(b.id)),
                }));
              }}
            >
              <Plus className="h-4 w-4" />
            </Button>
          </div>

          <div className="max-h-[560px] overflow-auto space-y-1">
            {configDraft.providers.map((provider) => {
              const active = provider.id === selectedProviderId;
              const count = providerModels[provider.id]?.length ?? 0;
              return (
                <button
                  key={provider.id}
                  type="button"
                  onClick={() => setSelectedProviderId(provider.id)}
                  className={`w-full text-left rounded px-2 py-1.5 border transition ${
                    active ? 'bg-secondary border-border' : 'border-transparent hover:bg-muted/50'
                  }`}
                >
                  <div className="flex items-center gap-2">
                    {createElement(providerIcon(provider), { className: 'h-3.5 w-3.5' })}
                    <div className="text-sm font-medium truncate">{provider.displayName || provider.id}</div>
                  </div>
                  <div className="text-[11px] text-muted-foreground">{PROVIDER_TYPE_LABEL[provider.providerType]} · {count} models</div>
                </button>
              );
            })}
          </div>
        </div>

        <div className="rounded border p-4 space-y-4">
          {!selectedProvider ? (
            <div className="text-sm text-muted-foreground">Select a provider to edit.</div>
          ) : (
            <ProviderDetail
              provider={selectedProvider}
              providerModels={selectedProviderModels}
              copilotAuth={copilotAuth}
              setCopilotAuth={setCopilotAuth}
              onStartCopilotOAuth={startCopilotOAuth}
              setDraft={setDraft}
              setSelectedProviderId={setSelectedProviderId}
            />
          )}
        </div>
      </div>
    </section>
  );
}

function ProviderDetail({
  provider,
  providerModels,
  copilotAuth,
  setCopilotAuth,
  onStartCopilotOAuth,
  setDraft,
  setSelectedProviderId,
}: {
  provider: UiProviderConfig;
  providerModels: api.ModelDetailsItem[];
  copilotAuth: { authenticated: boolean; loading: boolean; polling: boolean; userCode?: string; verificationUri?: string; message?: string };
  setCopilotAuth: React.Dispatch<React.SetStateAction<typeof copilotAuth>>;
  onStartCopilotOAuth: () => Promise<void>;
  setDraft: ConfigSectionProps['setDraft'];
  setSelectedProviderId: React.Dispatch<React.SetStateAction<string | null>>;
}) {
  const isCopilot = provider.providerType === 'copilot' || provider.id === 'copilot';

  return (
    <>
      <div className="flex items-center justify-between gap-2">
        <div>
          <div className="font-medium flex items-center gap-2">{createElement(providerIcon(provider), { className: 'h-4 w-4' })}{provider.id}</div>
          <div className="text-xs text-muted-foreground">{PROVIDER_TYPE_LABEL[provider.providerType]}</div>
        </div>
        <div className="flex items-center gap-2">
          <Label className="text-xs text-muted-foreground">Enabled</Label>
          <input
            type="checkbox"
            checked={provider.enabled}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                providers: prev.providers.map((p) =>
                  p.id === provider.id ? { ...p, enabled: e.target.checked } : p,
                ),
              }))
            }
          />
          <Button
            variant="ghost"
            size="icon"
            onClick={() => {
              setDraft((prev) => ({
                ...prev,
                providers: prev.providers.filter((p) => p.id !== provider.id),
              }));
              setSelectedProviderId((prev) => (prev === provider.id ? null : prev));
            }}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </div>

      <div className="grid gap-3 md:grid-cols-2">
        <div className="space-y-1">
          <Label>Type</Label>
          <Select
            value={provider.providerType}
            onValueChange={(v) =>
              setDraft((prev) => ({
                ...prev,
                providers: prev.providers.map((p) =>
                  p.id === provider.id ? { ...p, providerType: v as UiProviderConfig['providerType'] } : p,
                ),
              }))
            }
          >
            <SelectTrigger><SelectValue /></SelectTrigger>
            <SelectContent>
              {PROVIDER_TYPES.map((p) => (
                <SelectItem key={p} value={p}>{p}</SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="space-y-1">
          <Label>Display Name</Label>
          <Input
            value={provider.displayName ?? ''}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                providers: prev.providers.map((p) =>
                  p.id === provider.id ? { ...p, displayName: e.target.value || null } : p,
                ),
              }))
            }
            placeholder="optional"
          />
        </div>
      </div>

      <div className="rounded border bg-muted/10 p-3 space-y-2">
        <div className="text-xs text-muted-foreground">Available models ({providerModels.length})</div>
        <div className="max-h-44 overflow-auto rounded border p-2 text-xs space-y-1">
          {providerModels.length > 0 ? (
            providerModels.map((m) => (
              <div key={m.id} className="rounded border bg-background/70 px-2 py-1">
                <div className="font-mono">{m.id}</div>
                <div className="text-muted-foreground">{m.nickname}</div>
              </div>
            ))
          ) : (
            'No models fetched yet'
          )}
        </div>
      </div>

      {isCopilot ? (
        <div className="rounded border bg-muted/20 p-3 space-y-2">
          <div className="text-sm font-medium">GitHub Copilot OAuth</div>
          <div className="text-xs text-muted-foreground">
            {copilotAuth.authenticated
              ? 'Connected'
              : copilotAuth.polling
                ? 'Waiting for authorization...'
                : 'Not connected'}
          </div>
          {copilotAuth.userCode && copilotAuth.verificationUri && (
            <div className="text-xs">
              Code: <span className="font-mono">{copilotAuth.userCode}</span>{' '}
              <a className="underline" href={copilotAuth.verificationUri} target="_blank" rel="noreferrer">
                Open
              </a>
            </div>
          )}
          {copilotAuth.message && <div className="text-xs text-muted-foreground">{copilotAuth.message}</div>}
          <div className="flex gap-2">
            <Button size="sm" onClick={() => void onStartCopilotOAuth()} disabled={copilotAuth.loading || copilotAuth.polling}>
              <ExternalLink className="h-3 w-3 mr-1" /> Sign in
            </Button>
            <Button
              size="sm"
              variant="outline"
              onClick={async () => {
                await api.logoutCopilot();
                setCopilotAuth((s) => ({ ...s, authenticated: false, polling: false, message: 'Signed out' }));
              }}
            >
              Sign out
            </Button>
          </div>
        </div>
      ) : (
        <div className="grid gap-3 md:grid-cols-2">
          <div className="space-y-1">
            <Label>API Key</Label>
            <Input
              type="password"
              value={provider.apiKey ?? ''}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  providers: prev.providers.map((p) =>
                    p.id === provider.id ? { ...p, apiKey: e.target.value || null } : p,
                  ),
                }))
              }
              placeholder="optional for some providers"
            />
          </div>
          <div className="space-y-1">
            <Label>Base URL</Label>
            <Input
              value={provider.baseUrl ?? ''}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  providers: prev.providers.map((p) =>
                    p.id === provider.id ? { ...p, baseUrl: e.target.value || null } : p,
                  ),
                }))
              }
              placeholder="optional/custom provider"
            />
          </div>
        </div>
      )}
    </>
  );
}
