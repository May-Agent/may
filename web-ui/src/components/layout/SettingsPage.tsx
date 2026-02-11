import { useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Separator } from '@/components/ui/separator';
import { Textarea } from '@/components/ui/textarea';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import { Monitor, Type, Server, PlugZap, Wrench, Plus, Trash2, RotateCcw, BookOpen, AlertTriangle } from 'lucide-react';

interface SettingsPageProps {
  onClose: () => void;
}

export function SettingsPage({ onClose }: SettingsPageProps) {
  const {
    settings,
    updateSettings,
    uiConfig,
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

  const [sshName, setSshName] = useState('');
  const [sshTarget, setSshTarget] = useState('');
  const [mcpName, setMcpName] = useState('');
  const [mcpCommand, setMcpCommand] = useState('');
  const [mcpArgs, setMcpArgs] = useState('');
  const [mcpUrl, setMcpUrl] = useState('');
  const [skillRepo, setSkillRepo] = useState('');
  const [memoryDraft, setMemoryDraft] = useState('');
  const [memoryDirty, setMemoryDirty] = useState(false);

  useEffect(() => {
    void refreshMemory();
  }, [refreshMemory]);

  useEffect(() => {
    if (!memoryDirty) {
      setMemoryDraft(memoryContent);
    }
  }, [memoryContent, memoryDirty]);

  const sortedSkills = useMemo(
    () => [...skills].sort((a, b) => a.name.localeCompare(b.name)),
    [skills],
  );

  return (
    <div className="h-full overflow-auto bg-background">
      <div className="mx-auto max-w-5xl px-8 py-8 space-y-8">
        <div className="flex items-center justify-between">
          <h1 className="text-3xl font-semibold">Settings</h1>
          <Button variant="outline" onClick={onClose}>Back to chat</Button>
        </div>

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

        <Separator />

        <section className="space-y-4">
          <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
            <Type className="h-4 w-4" /> Runtime
          </div>
          <div className="text-sm text-muted-foreground">
            Max iterations: <span className="text-foreground font-medium">{uiConfig.maxIterations}</span>
          </div>
        </section>

        <Separator />

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

        <Separator />

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

        <Separator />

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

        <Separator />

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

        <Separator />

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
      </div>
    </div>
  );
}
