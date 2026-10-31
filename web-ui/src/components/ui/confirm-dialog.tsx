import { useState, type ReactNode } from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { Loader2 } from 'lucide-react';
import { Button } from './button';
import { DialogOverlay, DialogPortal } from './dialog';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: ReactNode;
  confirmLabel: string;
  /** Styled destructive; Cancel then takes the initial focus. */
  destructive?: boolean;
  /** Runs on confirm; the dialog shows progress until it settles, then closes. */
  onConfirm: () => void | Promise<void>;
}

/**
 * An alert in the HIG sense: a short title, one sentence of consequence,
 * Cancel and the action. Esc and Cancel dismiss it; a destructive action
 * never takes the default focus.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  destructive = false,
  onConfirm,
}: ConfirmDialogProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);

  const confirm = async () => {
    setBusy(true);
    try {
      await onConfirm();
      onOpenChange(false);
    } catch {
      // The caller reports the failure; the dialog stays open to retry.
    } finally {
      setBusy(false);
    }
  };

  return (
    <DialogPrimitive.Root open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogPortal>
        <DialogOverlay />
        <DialogPrimitive.Content
          role="alertdialog"
          className={cn(
            'fixed left-1/2 top-1/2 z-50 w-[calc(100%-2rem)] max-w-sm -translate-x-1/2 -translate-y-1/2',
            'rounded-xl border border-border bg-popover p-6 text-popover-foreground shadow-2xl',
            'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95',
          )}
          onOpenAutoFocus={(e) => {
            if (!destructive) return;
            // Keep focus off the destructive button.
            e.preventDefault();
            (e.currentTarget as HTMLElement)
              .querySelector<HTMLButtonElement>('[data-confirm-cancel]')
              ?.focus();
          }}
        >
          <DialogPrimitive.Title className="text-base font-semibold">{title}</DialogPrimitive.Title>
          {description ? (
            <DialogPrimitive.Description className="mt-2 text-sm text-muted-foreground">
              {description}
            </DialogPrimitive.Description>
          ) : null}
          <div className="mt-6 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <DialogPrimitive.Close asChild>
              <Button variant="outline" data-confirm-cancel disabled={busy}>
                {t('common.cancel')}
              </Button>
            </DialogPrimitive.Close>
            <Button
              variant={destructive ? 'destructive' : 'default'}
              onClick={() => void confirm()}
              disabled={busy}
              aria-busy={busy}
            >
              {busy && <Loader2 className="animate-spin" aria-hidden />}
              {confirmLabel}
            </Button>
          </div>
        </DialogPrimitive.Content>
      </DialogPortal>
    </DialogPrimitive.Root>
  );
}
