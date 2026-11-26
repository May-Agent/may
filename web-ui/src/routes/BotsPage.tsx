import { useEffect, useState } from 'react';
import {
  Activity,
  Bot,
  Check,
  Clock,
  FolderTree,
  GitCommit,
  Plus,
  Send,
  ShieldAlert,
  Square,
  Trash2,
  X,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import type { BotApprovalPolicy, BotMemoryNode, GatePolicy } from '@/lib/api/bots';
import { useBotsStore } from '@/lib/stores/bots';
import { cn } from '@/lib/utils';

type BotTab = 'chat' | 'approvals' | 'memory' | 'settings';

function flattenMemoryNodes(root: BotMemoryNode | null): BotMemoryNode[] {
  if (!root) return [];
  const result: BotMemoryNode[] = [];
  const walk = (node: BotMemoryNode) => {
    if (node.path) result.push(node);
    for (const child of node.children) walk(child);
  };
  walk(root);
  return result;
}

const GATE_OPTIONS: { value: GatePolicy; label: string }[] = [
  { value: 'auto_approve', label: 'Auto approve' },
  { value: 'bot_triage', label: 'Bot triage' },
  { value: 'always_escalate', label: 'Always escalate' },
];

export function BotsPage() {
  const bots = useBotsStore((s) => s.bots);
  const selectedBotId = useBotsStore((s) => s.selectedBotId);
  const loading = useBotsStore((s) => s.loading);
  const error = useBotsStore((s) => s.error);
  const messagesByBot = useBotsStore((s) => s.messages);
  const traceByBot = useBotsStore((s) => s.trace);
  const delegatesByBot = useBotsStore((s) => s.delegates);
  const approvalsByBot = useBotsStore((s) => s.approvals);
  const schedulesByBot = useBotsStore((s) => s.schedules);
  const memoryTreeByBot = useBotsStore((s) => s.memoryTree);
  const memoryHistoryByBot = useBotsStore((s) => s.memoryHistory);

  const loadBots = useBotsStore((s) => s.loadBots);
  const selectBot = useBotsStore((s) => s.selectBot);
  const createNewBot = useBotsStore((s) => s.createNewBot);
  const patchBot = useBotsStore((s) => s.patchBot);
  const removeBot = useBotsStore((s) => s.removeBot);
  const sendMessage = useBotsStore((s) => s.sendMessage);
  const stopDelegate = useBotsStore((s) => s.stopDelegate);
  const decideApproval = useBotsStore((s) => s.decideApproval);
  const removeSchedule = useBotsStore((s) => s.removeSchedule);

  const [tab, setTab] = useState<BotTab>('chat');
  const [showTrace, setShowTrace] = useState(false);
  const [draft, setDraft] = useState('');
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState('');
  const [newPersona, setNewPersona] = useState('');
  const [personaDraftByBot, setPersonaDraftByBot] = useState<Record<string, string>>({});

  useEffect(() => {
    void loadBots();
  }, [loadBots]);

  const selected = bots.find((b) => b.id === selectedBotId) ?? null;
  const personaDraft = selected ? (personaDraftByBot[selected.id] ?? selected.persona ?? '') : '';

  const messages = selected ? (messagesByBot[selected.id] ?? []) : [];
  const trace = selected ? (traceByBot[selected.id] ?? []) : [];
  const delegates = selected ? (delegatesByBot[selected.id] ?? []) : [];
  const approvals = selected ? (approvalsByBot[selected.id] ?? []) : [];
  const schedules = selected ? (schedulesByBot[selected.id] ?? []) : [];
  const memoryTree = flattenMemoryNodes(selected ? (memoryTreeByBot[selected.id] ?? null) : null);
  const memoryHistory = selected ? (memoryHistoryByBot[selected.id] ?? []) : [];

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newName.trim()) return;
    await createNewBot({
      name: newName.trim(),
      persona: newPersona.trim(),
      enabled: true,
    });
    setNewName('');
    setNewPersona('');
    setCreating(false);
  };

  const handleSend = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!selected || !draft.trim()) return;
    const text = draft.trim();
    setDraft('');
    await sendMessage(selected.id, text);
  };

  const handlePolicyChange = async (gate: keyof BotApprovalPolicy, value: GatePolicy) => {
    if (!selected) return;
    await patchBot(selected.id, {
      approvalPolicy: { ...selected.approvalPolicy, [gate]: value },
    });
  };

  return (
    <div className="flex h-full min-h-0 flex-1 overflow-hidden">
      {/* Left rail: Bot list */}
      <aside className="flex w-64 shrink-0 flex-col border-r border-border bg-muted/20">
        <div className="flex h-12 items-center justify-between border-b border-border px-3">
          <div className="flex items-center gap-2 text-sm font-semibold">
            <Bot className="size-4" aria-hidden />
            <span>May Bots</span>
          </div>
          <Button
            size="icon"
            variant="ghost"
            onClick={() => setCreating((v) => !v)}
            aria-label="New bot"
            title="New bot"
          >
            <Plus className="size-4" />
          </Button>
        </div>

        {creating && (
          <form onSubmit={(e) => void handleCreate(e)} className="space-y-2 border-b border-border p-3">
            <input
              type="text"
              placeholder="Bot name"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              className="w-full rounded-md border border-input bg-background px-2 py-1 text-xs"
              required
            />
            <textarea
              placeholder="Persona (optional)"
              value={newPersona}
              onChange={(e) => setNewPersona(e.target.value)}
              rows={2}
              className="w-full rounded-md border border-input bg-background px-2 py-1 text-xs"
            />
            <div className="flex justify-end gap-1">
              <Button type="button" size="sm" variant="ghost" onClick={() => setCreating(false)}>
                Cancel
              </Button>
              <Button type="submit" size="sm">
                Create
              </Button>
            </div>
          </form>
        )}

        <ScrollArea className="flex-1">
          <ul className="space-y-1 p-2">
            {loading && bots.length === 0 && (
              <li className="px-2 py-4 text-xs text-muted-foreground">Loading bots…</li>
            )}
            {!loading && bots.length === 0 && (
              <li className="px-2 py-4 text-xs text-muted-foreground">
                No resident bots yet. Click + to create one.
              </li>
            )}
            {bots.map((bot) => {
              const active = bot.id === selectedBotId;
              return (
                <li key={bot.id}>
                  <button
                    type="button"
                    onClick={() => void selectBot(bot.id)}
                    className={cn(
                      'flex w-full flex-col rounded-md px-2.5 py-2 text-left text-xs transition-colors hover:bg-accent',
                      active && 'bg-accent text-foreground',
                    )}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="truncate font-medium">{bot.name}</span>
                      <span
                        className={cn(
                          'rounded px-1.5 py-0.5 text-[10px] font-medium',
                          bot.runState === 'running'
                            ? 'bg-emerald-500/15 text-emerald-600 dark:text-emerald-400'
                            : bot.runState === 'disabled'
                              ? 'bg-muted text-muted-foreground'
                              : bot.runState === 'error'
                                ? 'bg-destructive/15 text-destructive'
                                : 'bg-sky-500/15 text-sky-600 dark:text-sky-400',
                        )}
                      >
                        {bot.runState}
                      </span>
                    </div>
                    <div className="mt-1 flex items-center gap-2 text-[11px] text-muted-foreground">
                      <span className="truncate font-mono">{bot.agentId}</span>
                      {bot.pendingApprovals > 0 && (
                        <span className="ml-auto rounded bg-amber-500/20 px-1 text-amber-600 dark:text-amber-400">
                          {bot.pendingApprovals} approval{bot.pendingApprovals === 1 ? '' : 's'}
                        </span>
                      )}
                    </div>
                  </button>
                </li>
              );
            })}
          </ul>
        </ScrollArea>
      </aside>

      {/* Main pane */}
      <div className="flex min-w-0 flex-1 flex-col">
        {error && (
          <div className="border-b border-destructive/30 bg-destructive/10 px-4 py-2 text-xs text-destructive">
            {error}
          </div>
        )}

        {!selected ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-center text-sm text-muted-foreground">
            <Bot className="size-8 opacity-50" />
            <p>Select or create a resident May Bot to start chatting.</p>
          </div>
        ) : (
          <>
            <header className="flex h-12 shrink-0 items-center justify-between border-b border-border px-4">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <h1 className="truncate text-sm font-semibold">{selected.name}</h1>
                  <span className="font-mono text-xs text-muted-foreground">
                    {selected.agentId}
                  </span>
                </div>
              </div>
              <div className="flex items-center gap-1">
                {(['chat', 'approvals', 'memory', 'settings'] as BotTab[]).map((key) => (
                  <Button
                    key={key}
                    size="sm"
                    variant={tab === key ? 'secondary' : 'ghost'}
                    onClick={() => setTab(key)}
                    className="h-7 text-xs capitalize"
                  >
                    {key}
                    {key === 'approvals' && selected.pendingApprovals > 0 && (
                      <span className="ml-1 rounded bg-amber-500/20 px-1 text-[10px]">
                        {selected.pendingApprovals}
                      </span>
                    )}
                  </Button>
                ))}
                {tab === 'chat' && (
                  <Button
                    size="sm"
                    variant={showTrace ? 'secondary' : 'outline'}
                    onClick={() => setShowTrace((v) => !v)}
                    className="ml-2 h-7 gap-1 text-xs"
                  >
                    <Activity className="size-3.5" />
                    Trace
                  </Button>
                )}
              </div>
            </header>

            {tab === 'chat' && (
              <div className="flex min-h-0 flex-1">
                <div className="flex min-w-0 flex-1 flex-col">
                  <ScrollArea className="flex-1 p-4">
                    <div className="mx-auto max-w-3xl space-y-3">
                      {messages.length === 0 && (
                        <p className="text-xs text-muted-foreground">
                          No user-visible messages yet. Only explicit{' '}
                          <code>send_message(to="user")</code> calls appear here.
                        </p>
                      )}
                      {messages.map((msg) => (
                        <div
                          key={msg.id}
                          className={cn(
                            'rounded-lg border px-3 py-2 text-sm',
                            msg.role === 'user'
                              ? 'ml-12 border-primary/20 bg-primary/5'
                              : 'mr-12 border-border bg-card',
                          )}
                        >
                          <div className="mb-1 flex items-center justify-between text-[11px] text-muted-foreground">
                            <span className="font-medium uppercase">{msg.role}</span>
                            <span className="font-mono">{msg.tsUtc}</span>
                          </div>
                          <div className="whitespace-pre-wrap break-words">{msg.content}</div>
                        </div>
                      ))}
                    </div>
                  </ScrollArea>

                  <form
                    onSubmit={(e) => void handleSend(e)}
                    className="flex items-center gap-2 border-t border-border p-3"
                  >
                    <input
                      type="text"
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      placeholder={`Message ${selected.name}…`}
                      className="flex-1 rounded-md border border-input bg-background px-3 py-2 text-sm"
                    />
                    <Button type="submit" size="sm" disabled={!draft.trim()}>
                      <Send className="mr-1.5 size-3.5" />
                      Send
                    </Button>
                  </form>
                </div>

                {showTrace && (
                  <aside className="flex w-80 shrink-0 flex-col border-l border-border bg-muted/10">
                    <div className="flex h-9 items-center justify-between border-b border-border px-3 text-xs font-medium">
                      <span>Internal Activity Trace</span>
                      <span className="text-muted-foreground">{trace.length}</span>
                    </div>
                    <ScrollArea className="flex-1 p-3">
                      <div className="space-y-2">
                        {delegates.length > 0 && (
                          <div className="space-y-1 border-b border-border pb-2">
                            <div className="text-[11px] font-semibold text-muted-foreground">
                              Delegated Workers
                            </div>
                            {delegates.map((d) => (
                              <div
                                key={d.agentId}
                                className="flex items-center justify-between rounded border border-border bg-background p-2 text-xs"
                              >
                                <div className="min-w-0">
                                  <div className="truncate font-medium">{d.name}</div>
                                  <div className="font-mono text-[10px] text-muted-foreground">
                                    {d.agentId} · {d.status}
                                  </div>
                                </div>
                                {d.status === 'running' && (
                                  <Button
                                    size="icon"
                                    variant="ghost"
                                    className="size-6"
                                    onClick={() => void stopDelegate(selected.id, d.agentId)}
                                    title="Stop delegated worker"
                                  >
                                    <Square className="size-3" />
                                  </Button>
                                )}
                              </div>
                            ))}
                          </div>
                        )}
                        {trace.map((entry) => (
                          <div
                            key={entry.id}
                            className="rounded border border-border bg-background p-2 text-xs"
                          >
                            <div className="mb-0.5 flex items-center justify-between text-[10px] text-muted-foreground">
                              <span className="font-mono uppercase">{entry.kind}</span>
                              <span>{entry.tsUtc}</span>
                            </div>
                            <div className="whitespace-pre-wrap break-words font-mono text-[11px]">
                              {entry.content}
                            </div>
                          </div>
                        ))}
                      </div>
                    </ScrollArea>
                  </aside>
                )}
              </div>
            )}

            {tab === 'approvals' && (
              <ScrollArea className="flex-1 p-4">
                <div className="mx-auto max-w-3xl space-y-3">
                  {approvals.length === 0 && (
                    <p className="text-xs text-muted-foreground">No approval requests recorded.</p>
                  )}
                  {approvals.map((item) => (
                    <div
                      key={item.requestId}
                      className="rounded-lg border border-border bg-card p-3 text-sm"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <div className="flex items-center gap-2">
                          <ShieldAlert className="size-4 text-amber-500" />
                          <span className="font-semibold uppercase">{item.gate}</span>
                          <span className="font-mono text-xs text-muted-foreground">
                            {item.requesterKind}:{item.requesterId}
                          </span>
                        </div>
                        <span className="rounded bg-muted px-2 py-0.5 text-xs">{item.status}</span>
                      </div>
                      <p className="mt-2 font-mono text-xs">{item.subject}</p>
                      {item.reason && (
                        <p className="mt-1 text-xs text-muted-foreground">Reason: {item.reason}</p>
                      )}
                      {item.decisionNote && (
                        <p className="mt-1 rounded bg-amber-500/10 px-2 py-1 text-xs text-amber-700 dark:text-amber-300">
                          Bot note: {item.decisionNote}
                        </p>
                      )}
                      {(item.status === 'escalated' || item.status === 'pending_bot') && (
                        <div className="mt-3 flex gap-2">
                          <Button
                            size="sm"
                            onClick={() => void decideApproval(selected.id, item.requestId, true)}
                          >
                            <Check className="mr-1 size-3.5" />
                            Approve
                          </Button>
                          <Button
                            size="sm"
                            variant="destructive"
                            onClick={() =>
                              void decideApproval(
                                selected.id,
                                item.requestId,
                                false,
                                'Denied by user',
                              )
                            }
                          >
                            <X className="mr-1 size-3.5" />
                            Deny
                          </Button>
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              </ScrollArea>
            )}

            {tab === 'memory' && (
              <div className="grid min-h-0 flex-1 grid-cols-1 divide-y divide-border md:grid-cols-2 md:divide-x md:divide-y-0">
                <ScrollArea className="p-4">
                  <div className="mb-2 flex items-center gap-2 text-xs font-semibold text-muted-foreground">
                    <FolderTree className="size-3.5" />
                    <span>Hierarchical Memory Tree</span>
                  </div>
                  <div className="space-y-2">
                    {memoryTree.map((node) => (
                      <div key={node.path} className="rounded border border-border bg-card p-2.5">
                        <div className="flex items-center justify-between text-xs font-medium">
                          <span className="font-mono">{node.path}</span>
                          {node.name === 'MEMORY.md' && (
                            <span className="rounded bg-sky-500/15 px-1.5 py-0.5 text-[10px] text-sky-600 dark:text-sky-400">
                              INDEX
                            </span>
                          )}
                        </div>
                        {node.content && (
                          <pre className="mt-1.5 overflow-x-auto whitespace-pre-wrap text-[11px] text-muted-foreground">
                            {node.content}
                          </pre>
                        )}
                      </div>
                    ))}
                  </div>
                </ScrollArea>

                <ScrollArea className="p-4">
                  <div className="mb-2 flex items-center gap-2 text-xs font-semibold text-muted-foreground">
                    <GitCommit className="size-3.5" />
                    <span>memory.git History</span>
                  </div>
                  <div className="space-y-2">
                    {memoryHistory.length === 0 && (
                      <p className="text-xs text-muted-foreground">No commits yet.</p>
                    )}
                    {memoryHistory.map((commit) => (
                      <div key={commit.sha} className="rounded border border-border bg-card p-2.5">
                        <div className="flex items-center justify-between text-xs">
                          <span className="font-medium">{commit.subject}</span>
                          <span className="font-mono text-[10px] text-muted-foreground">
                            {commit.sha.slice(0, 8)}
                          </span>
                        </div>
                        <div className="mt-0.5 text-[10px] text-muted-foreground">
                          {commit.timestamp} · {commit.author}
                        </div>
                        <pre className="mt-1.5 whitespace-pre-wrap text-[11px] text-muted-foreground">
                          {commit.message}
                        </pre>
                      </div>
                    ))}
                  </div>
                </ScrollArea>
              </div>
            )}

            {tab === 'settings' && (
              <ScrollArea className="flex-1 p-4">
                <div className="mx-auto max-w-2xl space-y-6">
                  <section className="space-y-2">
                    <h2 className="text-sm font-semibold">Live Configuration</h2>
                    <div className="flex items-center gap-3">
                      <Button
                        size="sm"
                        variant={selected.enabled ? 'secondary' : 'default'}
                        onClick={() =>
                          void patchBot(selected.id, { enabled: !selected.enabled })
                        }
                      >
                        {selected.enabled ? 'Pause Bot' : 'Enable Bot'}
                      </Button>
                      <Button
                        size="sm"
                        variant="destructive"
                        onClick={() => void removeBot(selected.id)}
                      >
                        <Trash2 className="mr-1.5 size-3.5" />
                        Delete Bot
                      </Button>
                    </div>
                  </section>

                  <section className="space-y-2">
                    <h2 className="text-sm font-semibold">Persona</h2>
                    <textarea
                      value={personaDraft}
                      onChange={(e) => {
                        const value = e.target.value;
                        setPersonaDraftByBot((prev) => ({ ...prev, [selected.id]: value }));
                      }}
                      rows={4}
                      className="w-full rounded-md border border-input bg-background p-2 text-xs"
                    />
                    <Button
                      size="sm"
                      onClick={() => {
                        void patchBot(selected.id, { persona: personaDraft }).then(() => {
                          setPersonaDraftByBot((prev) => {
                            const next = { ...prev };
                            delete next[selected.id];
                            return next;
                          });
                        });
                      }}
                    >
                      Save Persona
                    </Button>
                  </section>

                  <section className="space-y-2">
                    <h2 className="text-sm font-semibold">Approval Triage Policy</h2>
                    <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                      {(['unsafe', 'ssh', 'publish', 'workspace'] as const).map((gate) => (
                        <label
                          key={gate}
                          className="flex items-center justify-between rounded border border-border px-3 py-2 text-xs"
                        >
                          <span className="font-mono uppercase">{gate}</span>
                          <select
                            value={selected.approvalPolicy[gate]}
                            onChange={(e) =>
                              void handlePolicyChange(gate, e.target.value as GatePolicy)
                            }
                            className="rounded border border-input bg-background px-2 py-1 text-xs"
                          >
                            {GATE_OPTIONS.map((opt) => (
                              <option key={opt.value} value={opt.value}>
                                {opt.label}
                              </option>
                            ))}
                          </select>
                        </label>
                      ))}
                    </div>
                  </section>

                  <section className="space-y-2">
                    <h2 className="flex items-center gap-1.5 text-sm font-semibold">
                      <Clock className="size-4" />
                      Proactive Schedules
                    </h2>
                    {schedules.length === 0 ? (
                      <p className="text-xs text-muted-foreground">No proactive schedules set.</p>
                    ) : (
                      <ul className="space-y-1.5">
                        {schedules.map((s) => (
                          <li
                            key={s.id}
                            className="flex items-center justify-between rounded border border-border px-3 py-2 text-xs"
                          >
                            <div>
                              <div className="font-mono">
                                {s.kind}: {s.cronExpression ?? s.runAt ?? s.name}
                              </div>
                              <div className="text-muted-foreground">{s.prompt}</div>
                            </div>
                            <Button
                              size="icon"
                              variant="ghost"
                              onClick={() => void removeSchedule(selected.id, s.id)}
                              title="Delete schedule"
                            >
                              <Trash2 className="size-3.5" />
                            </Button>
                          </li>
                        ))}
                      </ul>
                    )}
                  </section>
                </div>
              </ScrollArea>
            )}
          </>
        )}
      </div>
    </div>
  );
}
