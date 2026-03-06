import { useState } from 'react';
import { Server, Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useStore } from '@/lib/store';

export function SshSection() {
  const { uiConfig, addSshServer, removeSshServer } = useStore();
  const [sshName, setSshName] = useState('');
  const [sshTarget, setSshTarget] = useState('');

  return (
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
  );
}
