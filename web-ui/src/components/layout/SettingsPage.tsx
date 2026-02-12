import { useEffect, useMemo, useState } from 'react';
import { OpenAI, Anthropic, Google } from '@lobehub/icons';
import { useSearchParams } from 'react-router-dom';
import * as api from '@/lib/api';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Separator } from '@/components/ui/separator';
import { Textarea } from '@/components/ui/textarea';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import type { UiFullConfig, UiProviderConfig, ProviderModelsItem, RequestLogEntry } from '@/lib/api';
import {
  Monitor,
  Server,
  PlugZap,
  Wrench,
  Plus,
  Trash2,
  RotateCcw,
  BookOpen,
  AlertTriangle,
  SlidersHorizontal,
  Boxes,
  ArrowUp,
  ArrowDown,
  ExternalLink,
} from 'lucide-react';


type SettingsSection =
  | 'appearance'
  | 'providers'
  | 'models'
  | 'preferences'
  | 'memory'
  | 'logs'
  | 'ssh'
  | 'mcp'
  | 'skills'
  | 'danger';

const PROVIDER_TYPES: UiProviderConfig['provider_type'][] = [
  'anthropic',
  'openai',
  'google',
  'copilot',
  'openrouter',
  'deepseek',
  'ollama',
  'groq',
  'together',
  'mistral',
  'xai',
  'custom',
];

const SECTIONS: Array<{ id: SettingsSection; label: string }> = [
  { id: 'appearance', label: 'Appearance' },
  { id: 'providers', label: 'Providers' },
  { id: 'models', label: 'Models' },
  { id: 'preferences', label: 'Preferences' },
  { id: 'memory', label: 'Memory' },
  { id: 'logs', label: 'Logs' },
  { id: 'ssh', label: 'SSH' },
  { id: 'mcp', label: 'MCP' },
  { id: 'skills', label: 'Skills' },
  { id: 'danger', label: 'Danger Zone' },
];

type TierKey = 'advanced' | 'balanced' | 'fast' | 'image_generation';

const TIER_LABELS: Record<TierKey, string> = {
  advanced: 'Advanced',
  balanced: 'Balanced',
  fast: 'Fast',
  image_generation: 'Image generation',
};

const PROVIDER_TYPE_LABEL: Record<UiProviderConfig['provider_type'], string> = {
  anthropic: 'Anthropic',
  openai: 'OpenAI',
  google: 'Google',
  copilot: 'Copilot',
  openrouter: 'OpenRouter',
  deepseek: 'DeepSeek',
  ollama: 'Ollama',
  groq: 'Groq',
  together: 'Together',
  mistral: 'Mistral',
  xai: 'xAI',
  custom: 'Custom',
};

export function SettingsPage() {
  const {
    settings,
    updateSettings,
    uiConfig,
    models,
    refreshFullConfig,
    refreshModelsFromProviders,
    saveFullConfig,
    addSshServer,
    removeSshServer,
    memoryContent,
    refreshMemory,
    saveMemory,
    deleteAllSessions,
    mcpServers,
    addMcpServer,
    removeMcpServer,
    reconnectMcpServer,
    skills,
    installSkill,
    deleteSkill,
  } = useStore();

  const [searchParams, setSearchParams] = useSearchParams();
  const activeSectionParam = searchParams.get('section') as SettingsSection | null;
  const activeSection: SettingsSection = SECTIONS.some((s) => s.id === activeSectionParam)
    ? (activeSectionParam as SettingsSection)
    : 'appearance';
  const [configDraft, setConfigDraft] = useState<UiFullConfig | null>(null);
  const [configDirty, setConfigDirty] = useState(false);

  const [sshName, setSshName] = useState('');
  const [sshTarget, setSshTarget] = useState('');
  const [mcpName, setMcpName] = useState('');
  const [mcpCommand, setMcpCommand] = useState('');
  const [mcpArgs, setMcpArgs] = useState('');
  const [mcpUrl, setMcpUrl] = useState('');
  const [skillRepo, setSkillRepo] = useState('');
  const [memoryDraft, setMemoryDraft] = useState('');
  const [memoryDirty, setMemoryDirty] = useState(false);

  const [newProviderType, setNewProviderType] = useState<UiProviderConfig['provider_type']>('openai');
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  const [modelSearch, setModelSearch] = useState('');
  const [modelsFetchLoading, setModelsFetchLoading] = useState(false);
  const [modelsFetchMessage, setModelsFetchMessage] = useState<string | null>(null);
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
  const [requestLogs, setRequestLogs] = useState<RequestLogEntry[]>([]);

  useEffect(() => {
    void refreshMemory();
    void refreshFullConfig();
  }, [refreshMemory, refreshFullConfig]);

  useEffect(() => {
    if (!memoryDirty) {
      setMemoryDraft(memoryContent);
    }
  }, [memoryContent, memoryDirty]);

  useEffect(() => {
    if (!configDirty) {
      setConfigDraft(uiConfig.fullConfig);
    }
  }, [uiConfig.fullConfig, configDirty]);

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
      } catch {
        // ignore
      }
    };

    void loadStatus();
    return () => {
      cancelled = true;
    };
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

    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [copilotAuth.polling, copilotAuth.deviceCode]);

  useEffect(() => {
    let cancelled = false;

    const loadProviderModels = async () => {
      try {
        const rows = await api.fetchModelsByProvider();
        if (cancelled) return;
        const grouped: Record<string, api.ModelDetailsItem[]> = {};
        for (const row of rows as ProviderModelsItem[]) {
          grouped[row.provider_id] =
            row.details && row.details.length > 0
              ? row.details
              : row.models.map((id) => ({ id, nickname: id, abilities: [] }));
        }
        setProviderModels(grouped);
      } catch {
      }
    };

    void loadProviderModels();

    return () => {
      cancelled = true;
    };
  }, [configDraft?.providers.length]);

  useEffect(() => {
    if (activeSection !== 'logs') return;

    let cancelled = false;

    const loadLogs = async () => {
      try {
        const logs = await api.fetchRequestLogs();
        if (!cancelled) setRequestLogs(logs);
      } catch {
      }
    };

    void loadLogs();
    const timer = window.setInterval(loadLogs, 3000);

    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [activeSection]);

  const sortedSkills = useMemo(
    () => [...skills].sort((a, b) => a.name.localeCompare(b.name)),
    [skills],
  );

  const setDraft = (updater: (prev: UiFullConfig) => UiFullConfig) => {
    setConfigDraft((prev) => {
      if (!prev) return prev;
      const next = updater(prev);
      setConfigDirty(true);
      void saveFullConfig(next)
        .then(() => setConfigDirty(false))
        .catch(() => {
          // keep dirty on failure
        });
      return next;
    });
  };

  const allModelIds = useMemo(() => {
    const ids = new Set(models.map((m) => m.id));
    if (configDraft) {
      configDraft.models.advanced.forEach((id) => ids.add(id));
      configDraft.models.balanced.forEach((id) => ids.add(id));
      configDraft.models.fast.forEach((id) => ids.add(id));
      configDraft.models.image_generation.forEach((id) => ids.add(id));
    }
    return Array.from(ids).sort((a, b) => a.localeCompare(b));
  }, [models, configDraft]);

  const filteredModelPool = useMemo(() => {
    const q = modelSearch.trim().toLowerCase();
    if (!q) return allModelIds;
    return allModelIds.filter((id) => id.toLowerCase().includes(q));
  }, [allModelIds, modelSearch]);

  const addModelToTier = (tier: TierKey, modelId: string) => {
    setDraft((prev) => {
      const current = prev.models[tier];
      if (current.includes(modelId)) return prev;
      return {
        ...prev,
        models: {
          ...prev.models,
          [tier]: [...current, modelId],
          model_providers: {
            ...prev.models.model_providers,
            [modelId]: prev.models.model_providers[modelId] ?? '',
          },
        },
      };
    });
  };

  const removeModelFromTier = (tier: TierKey, modelId: string) => {
    setDraft((prev) => ({
      ...prev,
      models: {
        ...prev.models,
        [tier]: prev.models[tier].filter((id) => id !== modelId),
      },
    }));
  };

  const moveModelInTier = (tier: TierKey, index: number, dir: -1 | 1) => {
    setDraft((prev) => {
      const list = [...prev.models[tier]];
      const next = index + dir;
      if (next < 0 || next >= list.length) return prev;
      const [item] = list.splice(index, 1);
      list.splice(next, 0, item);
      return {
        ...prev,
        models: {
          ...prev.models,
          [tier]: list,
        },
      };
    });
  };

  const startCopilotOAuth = async () => {
    setCopilotAuth((s) => ({ ...s, loading: true, message: undefined }));
    try {
      const start = await api.startCopilotAuth();
      setCopilotAuth((s) => ({
        ...s,
        loading: false,
        polling: true,
        deviceCode: start.device_code,
        userCode: start.user_code,
        verificationUri: start.verification_uri,
      }));
      window.open(start.verification_uri, '_blank', 'noopener,noreferrer');
    } catch (e) {
      setCopilotAuth((s) => ({ ...s, loading: false, message: (e as Error).message }));
    }
  };

  const selectedProvider = configDraft?.providers.find((p) => p.id === selectedProviderId) ?? null;
  const selectedProviderModels = selectedProvider ? providerModels[selectedProvider.id] ?? [] : [];

  const providerIcon = (provider: UiProviderConfig) => {
    switch (provider.provider_type) {
      case 'openai':
      case 'openrouter':
      case 'deepseek':
      case 'groq':
      case 'together':
      case 'mistral':
      case 'xai':
      case 'ollama':
      case 'custom':
      case 'copilot':
        return OpenAI;
      case 'anthropic':
        return Anthropic;
      case 'google':
        return Google;
      default:
        return OpenAI;
    }
  };

  const handleFetchModels = async () => {
    try {
      setModelsFetchLoading(true);
      setModelsFetchMessage(null);
      await refreshModelsFromProviders();
      const rows = await api.fetchModelsByProvider();
      const grouped: Record<string, api.ModelDetailsItem[]> = {};
      for (const row of rows as ProviderModelsItem[]) {
        grouped[row.provider_id] =
          row.details && row.details.length > 0
            ? row.details
            : row.models.map((id) => ({ id, nickname: id, abilities: [] }));
      }
      setProviderModels(grouped);
      setModelsFetchMessage('Models refreshed from providers');
    } catch (e) {
      setModelsFetchMessage((e as Error).message);
    } finally {
      setModelsFetchLoading(false);
    }
  };

  return (
    <div className="h-full overflow-hidden bg-background">
      <div className="flex h-full">
        <aside className="w-56 border-r bg-muted/20 p-4 flex flex-col gap-2">
          <div className="text-xs uppercase tracking-wider text-muted-foreground px-2 mb-1">Settings</div>
          {SECTIONS.map((section) => (
            <Button
              key={section.id}
              variant={activeSection === section.id ? 'secondary' : 'ghost'}
              className="justify-start"
              onClick={() => setSearchParams({ section: section.id })}
            >
              {section.label}
            </Button>
          ))}
        </aside>

        <div className="flex-1 overflow-auto">
          <div className="mx-auto max-w-5xl px-8 py-8 space-y-8">

            {activeSection === 'appearance' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <Monitor className="h-4 w-4" /> Appearance
                </div>
                <div className="grid gap-4 md:grid-cols-2">
                  <div className="space-y-2">
                    <Label>Theme</Label>
                    <Select
                      value={settings.theme}
                      onValueChange={(v) => {
                        const theme = v as 'light' | 'dark' | 'system';
                        updateSettings({ theme });
                        setDraft((prev) => ({
                          ...prev,
                          appearance: { ...prev.appearance, theme },
                        }));
                      }}
                    >
                      <SelectTrigger><SelectValue /></SelectTrigger>
                      <SelectContent>
                        <SelectItem value="light">Light</SelectItem>
                        <SelectItem value="dark">Dark</SelectItem>
                        <SelectItem value="system">System</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>

                  <div className="space-y-2">
                    <Label>Font</Label>
                    <Select
                      value={settings.font}
                      onValueChange={(v) => {
                        updateSettings({ font: v });
                        setDraft((prev) => ({
                          ...prev,
                          appearance: { ...prev.appearance, font: v },
                        }));
                      }}
                    >
                      <SelectTrigger><SelectValue /></SelectTrigger>
                      <SelectContent>
                        {FONTS.map((f) => (
                          <SelectItem key={f.value} value={f.value} style={{ fontFamily: f.family }}>
                            {f.name}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                </div>
              </section>
            )}

            {activeSection === 'providers' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <Boxes className="h-4 w-4" /> Providers
                </div>

                {!configDraft ? (
                  <div className="text-sm text-muted-foreground">Loading provider config...</div>
                ) : (
                  <div className="grid gap-4 md:grid-cols-[280px_1fr]">
                    <div className="rounded border bg-muted/20 p-2 space-y-2">
                      <div className="flex items-center gap-2">
                        <Select value={newProviderType} onValueChange={(v) => setNewProviderType(v as UiProviderConfig['provider_type'])}>
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
                                  provider_type: newProviderType,
                                  api_key: null,
                                  base_url: null,
                                  display_name: null,
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
                          const Icon = providerIcon(provider);
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
                                <Icon className="h-3.5 w-3.5" />
                                <div className="text-sm font-medium truncate">{provider.display_name || provider.id}</div>
                              </div>
                              <div className="text-[11px] text-muted-foreground">{PROVIDER_TYPE_LABEL[provider.provider_type]} · {count} models</div>
                            </button>
                          );
                        })}
                      </div>
                    </div>

                    <div className="rounded border p-4 space-y-4">
                      {!selectedProvider ? (
                        <div className="text-sm text-muted-foreground">Select a provider to edit.</div>
                      ) : (
                        (() => {
                          const provider = selectedProvider;
                          const isCopilot = provider.provider_type === 'copilot' || provider.id === 'copilot';
                          const Icon = providerIcon(provider);
                          return (
                            <>
                              <div className="flex items-center justify-between gap-2">
                                <div>
                                  <div className="font-medium flex items-center gap-2"><Icon className="h-4 w-4" />{provider.id}</div>
                                  <div className="text-xs text-muted-foreground">{PROVIDER_TYPE_LABEL[provider.provider_type]}</div>
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
                                    value={provider.provider_type}
                                    onValueChange={(v) =>
                                      setDraft((prev) => ({
                                        ...prev,
                                        providers: prev.providers.map((p) =>
                                          p.id === provider.id ? { ...p, provider_type: v as UiProviderConfig['provider_type'] } : p,
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
                                    value={provider.display_name ?? ''}
                                    onChange={(e) =>
                                      setDraft((prev) => ({
                                        ...prev,
                                        providers: prev.providers.map((p) =>
                                          p.id === provider.id ? { ...p, display_name: e.target.value || null } : p,
                                        ),
                                      }))
                                    }
                                    placeholder="optional"
                                  />
                                </div>
                              </div>

                              <div className="rounded border bg-muted/10 p-3 space-y-2">
                                <div className="text-xs text-muted-foreground">Available models ({selectedProviderModels.length})</div>
                                <div className="max-h-44 overflow-auto rounded border p-2 text-xs space-y-1">
                                  {selectedProviderModels.length > 0 ? (
                                    selectedProviderModels.map((m) => (
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
                                    <Button size="sm" onClick={() => void startCopilotOAuth()} disabled={copilotAuth.loading || copilotAuth.polling}>
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
                                      value={provider.api_key ?? ''}
                                      onChange={(e) =>
                                        setDraft((prev) => ({
                                          ...prev,
                                          providers: prev.providers.map((p) =>
                                            p.id === provider.id ? { ...p, api_key: e.target.value || null } : p,
                                          ),
                                        }))
                                      }
                                      placeholder="optional for some providers"
                                    />
                                  </div>
                                  <div className="space-y-1">
                                    <Label>Base URL</Label>
                                    <Input
                                      value={provider.base_url ?? ''}
                                      onChange={(e) =>
                                        setDraft((prev) => ({
                                          ...prev,
                                          providers: prev.providers.map((p) =>
                                            p.id === provider.id ? { ...p, base_url: e.target.value || null } : p,
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
                        })()
                      )}
                    </div>
                  </div>
                )}
              </section>
            )}

            {activeSection === 'models' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <SlidersHorizontal className="h-4 w-4" /> Models
                </div>

                {!configDraft ? (
                  <div className="text-sm text-muted-foreground">Loading model config...</div>
                ) : (
                  <div className="space-y-4">
                    <div className="space-y-1">
                      <div className="flex items-center justify-between">
                        <Label>Model Pool</Label>
                        <Button size="sm" variant="outline" onClick={() => void handleFetchModels()} disabled={modelsFetchLoading}>
                          {modelsFetchLoading ? 'Fetching…' : 'Fetch models'}
                        </Button>
                      </div>
                      <Input
                        value={modelSearch}
                        onChange={(e) => setModelSearch(e.target.value)}
                        placeholder="Search models..."
                      />
                      {modelsFetchMessage && (
                        <div className="text-xs text-muted-foreground">{modelsFetchMessage}</div>
                      )}
                      <div className="max-h-40 overflow-auto rounded border p-2 text-sm">
                        {filteredModelPool.map((id) => (
                          <div key={id} className="flex items-center justify-between py-1">
                            <span className="font-mono text-xs">{id}</span>
                            <div className="flex gap-1">
                              {(Object.keys(TIER_LABELS) as TierKey[]).map((tier) => (
                                <Button
                                  key={tier}
                                  size="sm"
                                  variant="outline"
                                  className="h-6 px-2 text-[10px]"
                                  onClick={() => addModelToTier(tier, id)}
                                >
                                  + {TIER_LABELS[tier]}
                                </Button>
                              ))}
                            </div>
                          </div>
                        ))}
                      </div>
                    </div>

                    {(Object.keys(TIER_LABELS) as TierKey[]).map((tier) => (
                      <div key={tier} className="space-y-1">
                        <Label>{TIER_LABELS[tier]}</Label>
                        <div className="rounded border divide-y">
                          {configDraft.models[tier].map((id, index) => (
                            <div key={`${tier}-${id}-${index}`} className="flex items-center justify-between px-3 py-2">
                              <span className="font-mono text-xs">{id}</span>
                              <div className="flex items-center gap-1">
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7"
                                  onClick={() => moveModelInTier(tier, index, -1)}
                                  disabled={index === 0}
                                >
                                  <ArrowUp className="h-3 w-3" />
                                </Button>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7"
                                  onClick={() => moveModelInTier(tier, index, 1)}
                                  disabled={index === configDraft.models[tier].length - 1}
                                >
                                  <ArrowDown className="h-3 w-3" />
                                </Button>
                                <Button
                                  variant="ghost"
                                  size="icon"
                                  className="h-7 w-7"
                                  onClick={() => removeModelFromTier(tier, id)}
                                >
                                  <Trash2 className="h-3 w-3" />
                                </Button>
                              </div>
                            </div>
                          ))}
                          {configDraft.models[tier].length === 0 && (
                            <div className="px-3 py-2 text-xs text-muted-foreground">No models in this tier</div>
                          )}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </section>
            )}

            {activeSection === 'preferences' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <SlidersHorizontal className="h-4 w-4" /> Preferences
                </div>

                {!configDraft ? (
                  <div className="text-sm text-muted-foreground">Loading preferences...</div>
                ) : (
                  <div className="grid gap-4 md:grid-cols-2">
                    <div className="space-y-1">
                      <Label>Load Balance Strategy</Label>
                      <Select
                        value={configDraft.preferences.load_balance_strategy}
                        onValueChange={(v) =>
                          setDraft((prev) => ({
                            ...prev,
                            preferences: {
                              ...prev.preferences,
                              load_balance_strategy: v as UiFullConfig['preferences']['load_balance_strategy'],
                            },
                          }))
                        }
                      >
                        <SelectTrigger><SelectValue /></SelectTrigger>
                        <SelectContent>
                          <SelectItem value="round_robin">round_robin</SelectItem>
                          <SelectItem value="failover_only">failover_only</SelectItem>
                          <SelectItem value="random">random</SelectItem>
                        </SelectContent>
                      </Select>
                    </div>

                    <div className="space-y-1">
                      <Label>Last model</Label>
                      <Input
                        value={configDraft.last_model ?? ''}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            last_model: e.target.value || null,
                          }))
                        }
                        placeholder="e.g. claude-sonnet-4"
                      />
                    </div>

                    <div className="space-y-1">
                      <Label>Max iterations</Label>
                      <Input
                        type="number"
                        min={1}
                        value={configDraft.max_iterations}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            max_iterations: Number(e.target.value || 1),
                          }))
                        }
                      />
                    </div>

                    <div className="space-y-1">
                      <Label className="text-sm">Warn on model change</Label>
                      <div className="pt-2">
                        <input
                          type="checkbox"
                          checked={configDraft.preferences.warn_on_model_change}
                          onChange={(e) =>
                            setDraft((prev) => ({
                              ...prev,
                              preferences: {
                                ...prev.preferences,
                                warn_on_model_change: e.target.checked,
                              },
                            }))
                          }
                        />
                      </div>
                    </div>

                    <div className="space-y-1">
                      <Label className="text-sm">Show token usage in chat</Label>
                      <div className="pt-2">
                        <input
                          type="checkbox"
                          checked={configDraft.preferences.show_token_usage}
                          onChange={(e) =>
                            setDraft((prev) => ({
                              ...prev,
                              preferences: {
                                ...prev.preferences,
                                show_token_usage: e.target.checked,
                              },
                            }))
                          }
                        />
                      </div>
                    </div>
                  </div>
                )}
              </section>
            )}

            {activeSection === 'memory' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <BookOpen className="h-4 w-4" /> Memory
                </div>
                <div className="space-y-2">
                  <Label>memory.md</Label>
                  <Textarea
                    value={memoryDraft}
                    onChange={(e) => {
                      setMemoryDraft(e.target.value);
                      setMemoryDirty(true);
                    }}
                    placeholder="Write memory context for future sessions..."
                    className="min-h-[220px] font-mono text-sm"
                  />
                  <div className="flex gap-2">
                    <Button
                      variant="outline"
                      onClick={async () => {
                        await refreshMemory();
                        setMemoryDirty(false);
                      }}
                    >
                      Reload
                    </Button>
                    <Button
                      onClick={async () => {
                        await saveMemory(memoryDraft);
                        setMemoryDirty(false);
                      }}
                      disabled={!memoryDirty}
                    >
                      Save memory
                    </Button>
                  </div>
                </div>
              </section>
            )}

            {activeSection === 'logs' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <BookOpen className="h-4 w-4" /> Request Logs
                </div>
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
                      <div className="text-muted-foreground">session={log.session_id}</div>
                      <div>model=<span className="font-mono">{log.model}</span> · provider=<span className="font-mono">{log.provider_id}</span></div>
                      <div>tokens: prompt={log.prompt_tokens ?? '-'} completion={log.completion_tokens ?? '-'} total={log.total_tokens ?? '-'}</div>
                      <div>latency: {log.elapsed_ms}ms</div>
                      {log.error && <div className="text-red-500 break-all">{log.error}</div>}
                    </div>
                  ))}
                </div>
              </section>
            )}

            {activeSection === 'ssh' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <Server className="h-4 w-4" /> SSH Servers
                </div>
                <div className="grid gap-2 md:grid-cols-3">
                  <Input value={sshName} onChange={(e) => setSshName(e.target.value)} placeholder="name" />
                  <Input value={sshTarget} onChange={(e) => setSshTarget(e.target.value)} placeholder="user@host" />
                  <Button
                    onClick={async () => {
                      if (!sshName.trim() || !sshTarget.trim()) return;
                      await addSshServer(sshName.trim(), sshTarget.trim());
                      setSshName('');
                      setSshTarget('');
                    }}
                  >
                    <Plus className="h-4 w-4 mr-2" /> Add
                  </Button>
                </div>
                <div className="space-y-2">
                  {uiConfig.sshServers.map((server) => (
                    <div key={server.name} className="flex items-center justify-between rounded border px-3 py-2">
                      <div>
                        <div className="font-medium">{server.name}</div>
                        <div className="text-xs text-muted-foreground">{server.target}</div>
                      </div>
                      <Button variant="ghost" size="icon" onClick={() => removeSshServer(server.name)}>
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  ))}
                </div>
              </section>
            )}

            {activeSection === 'mcp' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <PlugZap className="h-4 w-4" /> MCP Servers
                </div>
                <div className="grid gap-2 md:grid-cols-4">
                  <Input value={mcpName} onChange={(e) => setMcpName(e.target.value)} placeholder="name" />
                  <Input value={mcpCommand} onChange={(e) => setMcpCommand(e.target.value)} placeholder="command (optional)" />
                  <Input value={mcpArgs} onChange={(e) => setMcpArgs(e.target.value)} placeholder="args e.g. --foo bar" />
                  <Input value={mcpUrl} onChange={(e) => setMcpUrl(e.target.value)} placeholder="url (optional)" />
                </div>
                <Button
                  onClick={async () => {
                    if (!mcpName.trim()) return;
                    await addMcpServer({
                      name: mcpName.trim(),
                      command: mcpCommand.trim() || undefined,
                      args: mcpArgs.trim() ? mcpArgs.trim().split(/\s+/) : undefined,
                      url: mcpUrl.trim() || undefined,
                    });
                    setMcpName('');
                    setMcpCommand('');
                    setMcpArgs('');
                    setMcpUrl('');
                  }}
                >
                  <Plus className="h-4 w-4 mr-2" /> Add MCP Server
                </Button>
                <div className="space-y-2">
                  {mcpServers.map((server) => (
                    <div key={server.name} className="flex items-center justify-between rounded border px-3 py-2">
                      <div>
                        <div className="font-medium">{server.name}</div>
                        <div className="text-xs text-muted-foreground">{server.source}</div>
                        <div className="text-xs">{server.status}</div>
                      </div>
                      <div className="flex gap-2">
                        <Button variant="outline" size="icon" onClick={() => reconnectMcpServer(server.name)}>
                          <RotateCcw className="h-4 w-4" />
                        </Button>
                        <Button variant="ghost" size="icon" onClick={() => removeMcpServer(server.name)}>
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </div>
                  ))}
                </div>
              </section>
            )}

            {activeSection === 'skills' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
                  <Wrench className="h-4 w-4" /> Skills
                </div>
                <div className="flex gap-2">
                  <Input value={skillRepo} onChange={(e) => setSkillRepo(e.target.value)} placeholder="owner/repo" />
                  <Button
                    onClick={async () => {
                      if (!skillRepo.trim()) return;
                      await installSkill(skillRepo.trim());
                      setSkillRepo('');
                    }}
                  >
                    <Plus className="h-4 w-4 mr-2" /> Install
                  </Button>
                </div>
                <div className="space-y-2">
                  {sortedSkills.map((skill) => (
                    <div key={skill.dir_name} className="flex items-center justify-between rounded border px-3 py-2">
                      <div>
                        <div className="font-medium">{skill.name}</div>
                        <div className="text-xs text-muted-foreground">{skill.description}</div>
                        <div className="text-xs text-muted-foreground">{skill.source}</div>
                      </div>
                      <Button variant="ghost" size="icon" onClick={() => deleteSkill(skill.dir_name)}>
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  ))}
                </div>
              </section>
            )}

            {activeSection === 'danger' && (
              <section className="space-y-4">
                <div className="flex items-center gap-2 text-sm font-medium uppercase text-destructive tracking-wide">
                  <AlertTriangle className="h-4 w-4" /> Danger Zone
                </div>
                <div className="rounded border border-destructive/40 bg-destructive/5 p-4 flex flex-col gap-3 md:flex-row md:items-center md:justify-between">
                  <div>
                    <div className="font-medium">Delete all sessions</div>
                    <div className="text-xs text-muted-foreground">This removes every saved chat session and creates a fresh empty one.</div>
                  </div>
                  <Button
                    variant="destructive"
                    onClick={async () => {
                      if (!window.confirm('Delete all sessions? This cannot be undone.')) return;
                      await deleteAllSessions();
                    }}
                  >
                    Delete all sessions
                  </Button>
                </div>
              </section>
            )}

            <Separator />
          </div>
        </div>
      </div>
    </div>
  );
}
