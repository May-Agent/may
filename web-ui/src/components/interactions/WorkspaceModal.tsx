import { useState } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import type { PersistedWorkspaceRequest } from '@/lib/types';
import { respondWorkspace } from '@/lib/api/interactions';
import { useTranslation } from '@/lib/i18n';

export function WorkspaceModal({ request }: { request: PersistedWorkspaceRequest | null }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  if (!request) return null;

  const decide = async (approved: boolean) => {
    setBusy(true);
    try {
      await respondWorkspace(request.id, approved);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('modals.workspace.title')}</DialogTitle>
          <DialogDescription>{t('modals.workspace.description')}</DialogDescription>
        </DialogHeader>
        <div className="space-y-2 py-3">
          <div>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1">
              {t('modals.workspace.path')}
            </div>
            <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono break-all">
              {request.path}
            </pre>
          </div>
          <div>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1">
              {t('modals.workspace.reason')}
            </div>
            <div className="text-sm">{request.reason}</div>
          </div>
        </div>
        <div className="flex justify-end gap-2">
          <Button variant="outline" disabled={busy} onClick={() => decide(false)}>
            {t('common.deny')}
          </Button>
          <Button disabled={busy} onClick={() => decide(true)}>
            {t('common.approve')}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
