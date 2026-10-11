import { useState } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import type { PersistedPermissionRequest, TerminalCommandMode } from '@/lib/types';
import { respondPermission } from '@/lib/api/interactions';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { ShieldAlert, ShieldCheck, Shield } from 'lucide-react';
import { cn } from '@/lib/utils';

const MODE_LABEL: Record<TerminalCommandMode, MessageKey> = {
  default: 'modals.permission.modeDefault',
  sandboxed: 'modals.permission.modeSandboxed',
  unsafe: 'modals.permission.modeUnsafe',
  ssh: 'modals.permission.modeRemote',
};

const MODE_ICON: Record<TerminalCommandMode, typeof Shield> = {
  default: Shield,
  sandboxed: ShieldCheck,
  unsafe: ShieldAlert,
  ssh: Shield,
};

const MODE_CLASS: Record<TerminalCommandMode, string> = {
  default: 'text-muted-foreground',
  sandboxed: 'text-emerald-600 dark:text-emerald-400',
  unsafe: 'text-amber-600 dark:text-amber-400',
  ssh: 'text-muted-foreground',
};

interface PermissionModalProps {
  request: PersistedPermissionRequest | null;
}

export function PermissionModal({ request }: PermissionModalProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  if (!request) return null;
  const mode = request.mode as TerminalCommandMode;
  const ModeIcon = MODE_ICON[mode] ?? Shield;
  const modeLabelKey = MODE_LABEL[mode] ?? MODE_LABEL.default;
  const modeColorClass = MODE_CLASS[mode] ?? MODE_CLASS.default;

  const decide = async (approved: boolean) => {
    setBusy(true);
    try {
      await respondPermission(request.id, approved);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('modals.permission.title')}</DialogTitle>
          <DialogDescription>{t('modals.permission.description')}</DialogDescription>
        </DialogHeader>
        <div className="space-y-2 py-3">
          <div className="text-xs uppercase tracking-wider text-muted-foreground">
            {t('modals.permission.command')}
          </div>
          <pre className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs font-mono whitespace-pre-wrap break-all">
            {request.script}
          </pre>
          <div className={cn('flex items-center gap-1.5 text-xs', modeColorClass)}>
            <ModeIcon className="size-3.5" />
            <span className="text-muted-foreground">{t('modals.permission.mode')}:</span>
            <span className="font-medium">{t(modeLabelKey)}</span>
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
