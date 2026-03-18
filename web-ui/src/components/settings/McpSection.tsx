import { useState } from 'react';
import { PlugZap, Plus, Trash2, RotateCcw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useStore } from '@/lib/store';
import type { McpServerInfo } from '@/lib/api';

function mcpSourceLabel(source: McpServerInfo['source']): string {
  switch (source.kind) {
    case 'url':
      return source.url;
    case 'process':
      return [source.command, ...source.args].join(' ').trim();
    case 'unknown':
      return 'Unknown';
  }
}

function mcpStatusLabel(status: McpServerInfo['status']): string {
  switch (status.state) {
    case 'disconnected':
      return 'Disconnected';
    case 'connecting':
      return 'Connecting...';
    case 'connected':
      return `Connected (${status.toolCount} tools)`;
    case 'error':
      return `Error: ${status.message}`;
  }
}

export function McpSection() {
  const { mcpServers, addMcpServer, removeMcpServer, reconnectMcpServer } = useStore();
  const [mcpName, setMcpName] = useState('');
  const [mcpCommand, setMcpCommand] = useState('');
  const [mcpArgs, setMcpArgs] = useState('');
  const [mcpUrl, setMcpUrl] = useState('');

  return (
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
              <div className="text-xs text-muted-foreground">{mcpSourceLabel(server.source)}</div>
              <div className="text-xs">{mcpStatusLabel(server.status)}</div>
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
  );
}
