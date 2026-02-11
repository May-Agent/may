import { useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Separator } from '@/components/ui/separator';
import { Textarea } from '@/components/ui/textarea';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import type { UiFullConfig, UiProviderConfig } from '@/lib/api';
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
} from 'lucide-react';

interface SettingsPageProps {
  onClose: () => void;
}

type SettingsSection =
  | 'appearance'
  | 'providers'
  | 'models'
  | 'preferences'
  | 'memory'
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
  { id: 'ssh', label: 'SSH' },
  { id: 'mcp', label: 'MCP' },
  { id: 'skills', label: 'Skills' },
  { id: 'danger', label: 'Danger Zone' },
];

function parseCsv(text: string): string[] {
  return text
    .split(',')
    .map((v) => v.trim())
    .filter(Boolean);
}

export function SettingsPage({ onClose }: SettingsPageProps) {
  const {
    settings,
    updateSettings,
    uiConfig,
    refreshFullConfig,
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

  const [activeSection, setActiveSection] = useState<SettingsSection>('appearance');
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

  const [newProviderId, setNewProviderId] = useState('');
  const [newProviderType, setNewProviderType] = useState<UiProviderConfig['provider_type']>('openai');

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

  const sortedSkills = useMemo(
    () => [...skills].sort((a, b) => a.name.localeCompare(b.name)),
    [skills],
  );

  const modelCsv = useMemo(() => {
    if (!configDraft) return { advanced: '', balanced: '', fast: '', image: '' };
    return {
      advanced: configDraft.models.advanced.join(', '),
      balanced: configDraft.models.balanced.join(', '),
      fast: configDraft.models.fast.join(', '),
      image: configDraft.models.image_generation.join(', '),
    };
  }, [configDraft]);

  const setDraft = (updater: (prev: UiFullConfig) => UiFullConfig) => {
    setConfigDraft((prev) => {
      if (!prev) return prev;
      setConfigDirty(true);
      return updater(prev);
    });
  };

  const saveConfig = async () => {
    if (!configDraft) return;
    await saveFullConfig(configDraft);
    setConfigDirty(false);
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
              onClick={() => setActiveSection(section.id)}
            >
              {section.label}
            </Button>
          ))}
        </aside>

        <div className="flex-1 overflow-auto">
          <div className="mx-auto max-w-5xl px-8 py-8 space-y-8">
            <div className="flex items-center justify-between">
              <h1 className="text-3xl font-semibold">Settings</h1>
              <div className="flex gap-2">
                {configDraft && (
                  <>
                    <Button
                      variant="outline"
                      onClick={async () => {
                        await refreshFullConfig();
                        setConfigDirty(false);
                      }}
                    >
                      Reload config
                    </Button>
                    <Button onClick={() => void saveConfig()} disabled={!configDirty}>
                      Save config
                    </Button>
                  </>
                )}
                <Button variant="outline" onClick={onClose}>Back to chat</Button>
              </div>
            </div>

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
                      onValueChange={(v) => updateSettings({ theme: v as 'light' | 'dark' | 'system' })}
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
                    <Select value={settings.font} onValueChange={(v) => updateSettings({ font: v })}>
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
                  <>
                    <div className="grid gap-2 md:grid-cols-4">
                      <Input
                        value={newProviderId}
                        onChange={(e) => setNewProviderId(e.target.value)}
                        placeholder="provider id (e.g. openai-main)"
                      />
                      <Select value={newProviderType} onValueChange={(v) => setNewProviderType(v as UiProviderConfig['provider_type'])}>
                        <SelectTrigger><SelectValue /></SelectTrigger>
                        <SelectContent>
                          {PROVIDER_TYPES.map((p) => (
                            <SelectItem key={p} value={p}>{p}</SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                      <div className="md:col-span-2">
                        <Button
                          onClick={() => {
                            if (!newProviderId.trim()) return;
                            setDraft((prev) => ({
                              ...prev,
                              providers: [
                                ...prev.providers.filter((p) => p.id !== newProviderId.trim()),
                                {
                                  id: newProviderId.trim(),
                                  provider_type: newProviderType,
                                  api_key: null,
                                  base_url: null,
                                  display_name: null,
                                  enabled: true,
                                },
                              ].sort((a, b) => a.id.localeCompare(b.id)),
                            }));
                            setNewProviderId('');
                          }}
                        >
                          <Plus className="h-4 w-4 mr-2" /> Add Provider
                        </Button>
                      </div>
                    </div>

                    <div className="space-y-3">
                      {configDraft.providers.map((provider) => (
                        <div key={provider.id} className="rounded border p-3 space-y-3">
                          <div className="flex items-center justify-between gap-2">
                            <div className="font-medium">{provider.id}</div>
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
                                onClick={() =>
                                  setDraft((prev) => ({
                                    ...prev,
                                    providers: prev.providers.filter((p) => p.id !== provider.id),
                                  }))
                                }
                              >
                                <Trash2 className="h-4 w-4" />
                              </Button>
                            </div>
                          </div>

                          <div className="grid gap-2 md:grid-cols-2">
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

                          <div className="grid gap-2 md:grid-cols-2">
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
                        </div>
                      ))}
                    </div>
                  </>
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
                      <Label>Advanced models (comma separated)</Label>
                      <Textarea
                        value={modelCsv.advanced}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            models: { ...prev.models, advanced: parseCsv(e.target.value) },
                          }))
                        }
                        className="min-h-[80px]"
                      />
                    </div>
                    <div className="space-y-1">
                      <Label>Balanced models (comma separated)</Label>
                      <Textarea
                        value={modelCsv.balanced}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            models: { ...prev.models, balanced: parseCsv(e.target.value) },
                          }))
                        }
                        className="min-h-[80px]"
                      />
                    </div>
                    <div className="space-y-1">
                      <Label>Fast models (comma separated)</Label>
                      <Textarea
                        value={modelCsv.fast}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            models: { ...prev.models, fast: parseCsv(e.target.value) },
                          }))
                        }
                        className="min-h-[80px]"
                      />
                    </div>
                    <div className="space-y-1">
                      <Label>Image generation models (comma separated)</Label>
                      <Textarea
                        value={modelCsv.image}
                        onChange={(e) =>
                          setDraft((prev) => ({
                            ...prev,
                            models: { ...prev.models, image_generation: parseCsv(e.target.value) },
                          }))
                        }
                        className="min-h-[80px]"
                      />
                    </div>
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
