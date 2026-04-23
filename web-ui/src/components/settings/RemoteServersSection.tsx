import { useEffect, useState } from 'react';
import { Plus, Trash2, Pencil } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { useConfigStore } from '@/lib/stores/config';
import {
  deleteSshServer,
  upsertSshServer,
  type UiSshServerConfig,
} from '@/lib/api/config';

interface DraftState {
  mode: 'create' | 'edit';
  originalName: string | null;
  name: string;
  target: string;
}

const EMPTY_DRAFT: DraftState = {
  mode: 'create',
  originalName: null,
  name: '',
  target: '',
};

export function RemoteServersSection() {
  const { t } = useTranslation();
  const config = useConfigStore((s) => s.config);
  const refresh = useConfigStore((s) => s.refresh);
  const servers = config?.sshServers ?? [];
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<DraftState>(EMPTY_DRAFT);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!config) void refresh();
  }, [config, refresh]);

  const openCreate = () => {
    setDraft(EMPTY_DRAFT);
    setOpen(true);
  };

  const openEdit = (server: UiSshServerConfig) => {
    setDraft({
      mode: 'edit',
      originalName: server.name,
      name: server.name,
      target: server.target,
    });
    setOpen(true);
  };

  const handleSubmit = async () => {
    const name = draft.name.trim();
    const target = draft.target.trim();
    if (!name) {
      toast.error(t('settings.remoteServers.nameRequired'));
      return;
    }
    if (!target) {
      toast.error(t('settings.remoteServers.targetRequired'));
      return;
    }
    setBusy(true);
    try {
      if (draft.mode === 'edit' && draft.originalName && draft.originalName !== name) {
        await deleteSshServer(draft.originalName);
      }
      await upsertSshServer({ name, target });
      await refresh();
      setOpen(false);
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const handleDelete = async (name: string) => {
    if (!window.confirm(t('settings.remoteServers.deleteConfirm'))) return;
    setBusy(true);
    try {
      await deleteSshServer(name);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <SectionHeader
        title={t('settings.remoteServers.title')}
        description={t('settings.remoteServers.description')}
        action={
          <Button size="sm" onClick={openCreate}>
            <Plus className="size-3.5" />
            {t('settings.remoteServers.addButton')}
          </Button>
        }
      />

      {servers.length === 0 ? (
        <div className="py-10 text-center text-sm text-muted-foreground">
          {t('settings.remoteServers.empty')}
        </div>
      ) : (
        <ul className="space-y-2">
          {servers.map((server) => (
            <li
              key={server.name}
              className="flex items-center justify-between rounded-md border border-border px-3 py-2 text-sm"
            >
              <div className="min-w-0">
                <div className="truncate font-medium">{server.name}</div>
                <div className="truncate font-mono text-xs text-muted-foreground">
                  {server.target}
                </div>
              </div>
              <div className="flex gap-1">
                <Button
                  size="icon"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => openEdit(server)}
                  aria-label={t('common.edit')}
                >
                  <Pencil className="size-4" />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => void handleDelete(server.name)}
                  aria-label={t('common.delete')}
                >
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('settings.remoteServers.title')}</DialogTitle>
            <DialogDescription>
              {t('settings.remoteServers.description')}
            </DialogDescription>
          </DialogHeader>
          <div className="mt-2 space-y-4">
            <div className="space-y-1.5">
              <Label>{t('settings.remoteServers.nameLabel')}</Label>
              <Input
                autoFocus
                value={draft.name}
                onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                placeholder={t('settings.remoteServers.namePlaceholder')}
              />
            </div>
            <div className="space-y-1.5">
              <Label>{t('settings.remoteServers.targetLabel')}</Label>
              <Input
                value={draft.target}
                onChange={(e) => setDraft({ ...draft, target: e.target.value })}
                placeholder={t('settings.remoteServers.targetPlaceholder')}
                className="font-mono"
              />
            </div>
          </div>
          <div className="mt-6 flex justify-end gap-2">
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setOpen(false)}
              disabled={busy}
            >
              {t('settings.remoteServers.cancel')}
            </Button>
            <Button size="sm" onClick={() => void handleSubmit()} disabled={busy}>
              {t('settings.remoteServers.save')}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </section>
  );
}
