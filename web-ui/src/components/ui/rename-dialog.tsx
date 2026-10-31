import { useId, useState } from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { Loader2 } from 'lucide-react';
import { Button } from './button';
import { DialogOverlay, DialogPortal } from './dialog';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';

interface RenameDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  label: string;
  initialValue: string;
  onSubmit: (value: string) => Promise<void>;
}

/** A small sheet with one text field; Return saves, Esc cancels. */
export function RenameDialog(props: RenameDialogProps) {
  // Mounted only while open, so the field starts from the current name.
  return props.open ? <OpenRenameDialog {...props} /> : null;
}

function OpenRenameDialog({
  open,
  onOpenChange,
  title,
  label,
  initialValue,
  onSubmit,
}: RenameDialogProps) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initialValue);
  const [busy, setBusy] = useState(false);
  const inputId = useId();

  const next = value.trim();
  const canSave = next.length > 0 && next !== initialValue && !busy;

  const save = async () => {
    if (!canSave) return;
    setBusy(true);
    try {
      await onSubmit(next);
      onOpenChange(false);
    } catch {
      // The caller reports the failure; the dialog stays open to retry.
    } finally {
      setBusy(false);
    }
  };

  return (
    <DialogPrimitive.Root open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <DialogPortal>
        <DialogOverlay />
        <DialogPrimitive.Content
          className={cn(
            'fixed left-1/2 top-1/2 z-50 w-[calc(100%-2rem)] max-w-sm -translate-x-1/2 -translate-y-1/2',
            'rounded-xl border border-border bg-popover p-6 text-popover-foreground shadow-2xl',
            'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95',
          )}
        >
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void save();
            }}
          >
            <DialogPrimitive.Title className="text-base font-semibold">{title}</DialogPrimitive.Title>
            <DialogPrimitive.Description className="sr-only">{label}</DialogPrimitive.Description>
            <label htmlFor={inputId} className="mt-4 block text-[13px] font-medium text-muted-foreground">
              {label}
            </label>
            <input
              id={inputId}
              value={value}
              onChange={(e) => setValue(e.target.value)}
              autoFocus
              onFocus={(e) => e.currentTarget.select()}
              className="mt-1.5 h-9 w-full rounded-md border border-input bg-background px-3 text-sm outline-none focus-visible:ring-2 focus-visible:ring-ring touch:h-11 touch:text-base"
            />
            <div className="mt-6 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
              <DialogPrimitive.Close asChild>
                <Button type="button" variant="outline" disabled={busy}>
                  {t('common.cancel')}
                </Button>
              </DialogPrimitive.Close>
              <Button type="submit" disabled={!canSave} aria-busy={busy}>
                {busy && <Loader2 className="animate-spin" aria-hidden />}
                {t('common.save')}
              </Button>
            </div>
          </form>
        </DialogPrimitive.Content>
      </DialogPortal>
    </DialogPrimitive.Root>
  );
}
