import { useRef, useState } from 'react';
import { Check, ChevronDown, Cloud, Loader2, Monitor } from 'lucide-react';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTranslation } from '@/lib/i18n';
import { getCloudCatalog } from '@/lib/api/profiles';
import { useProfilesStore } from '@/lib/stores/profiles';
import { useSessionsStore } from '@/lib/stores/sessions';
import type { MachineType } from '@/lib/types';
import { cn } from '@/lib/utils';

/** Bytes in a GiB, the unit GitHub's machine types are sized in. */
const GIB = 1024 ** 3;

type Catalog =
  | { state: 'loading' }
  | { state: 'loaded'; machines: MachineType[] }
  | { state: 'failed'; error: string };

/**
 * Where a new chat runs: this machine, or a codespace of the machine type
 * picked here. Shown only on a draft whose profile has Codespaces set up.
 */
export function MachinePicker({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [catalog, setCatalog] = useState<Catalog>({ state: 'loading' });
  const draft = useSessionsStore((s) => s.drafts[sessionId]);
  const setDraftMachine = useSessionsStore((s) => s.setDraftMachine);
  const profileId = draft?.session.profileId ?? null;
  const repo = draft?.session.repo ?? null;
  const hasCodespaces = useProfilesStore(
    (s) => s.profiles.find((p) => p.id === profileId)?.cloud.codespaces != null,
  );

  // Only the latest opening's answer is shown.
  const request = useRef(0);

  const onOpenChange = (next: boolean) => {
    setOpen(next);
    if (!next || !profileId) return;
    const current = ++request.current;
    setCatalog({ state: 'loading' });
    getCloudCatalog(profileId, repo)
      .then((machines) => {
        if (request.current === current) setCatalog({ state: 'loaded', machines });
      })
      .catch((err: unknown) => {
        if (request.current === current)
          setCatalog({ state: 'failed', error: err instanceof Error ? err.message : String(err) });
      });
  };

  if (!draft || !hasCodespaces) return null;

  const chosen = draft.machineType;
  const chosenLabel =
    chosen === null
      ? t('composer.machinePicker.thisMachine')
      : catalog.state === 'loaded'
        ? (catalog.machines.find((m) => m.name === chosen)?.displayName ?? chosen)
        : chosen;

  const choose = (machineType: string | null) => {
    setDraftMachine(sessionId, machineType);
    setOpen(false);
  };

  const row = (key: string, selected: boolean, onSelect: () => void, label: string, detail?: string) => (
    <button
      key={key}
      type="button"
      onClick={onSelect}
      className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent"
    >
      <Check className={cn('size-3.5 shrink-0', selected ? 'opacity-100' : 'opacity-0')} />
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {detail && <span className="shrink-0 text-xs text-muted-foreground">{detail}</span>}
    </button>
  );

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          aria-label={t('composer.machinePicker.label')}
          className="inline-flex h-7 max-w-[12rem] items-center gap-1 rounded-full bg-muted px-3 text-xs text-foreground transition-colors hover:bg-muted/70 disabled:pointer-events-none disabled:opacity-50"
        >
          {chosen === null ? (
            <Monitor className="size-3 shrink-0 opacity-60" aria-hidden />
          ) : (
            <Cloud className="size-3 shrink-0 opacity-60" aria-hidden />
          )}
          <span className="truncate">{chosenLabel}</span>
          <ChevronDown className="size-3 shrink-0 opacity-60" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[min(22rem,calc(100vw-2rem))] p-1">
        {row('local', chosen === null, () => choose(null), t('composer.machinePicker.thisMachine'))}
        <p className="px-2 pt-2 pb-1 text-xs text-muted-foreground">
          {t('composer.machinePicker.codespace')}
        </p>
        {catalog.state === 'loading' && (
          <div className="flex items-center gap-2 px-2 py-1.5 text-sm text-muted-foreground" role="status">
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            {t('composer.machinePicker.loading')}
          </div>
        )}
        {catalog.state === 'failed' && (
          <p className="px-2 py-1.5 text-sm text-destructive">
            {t('composer.machinePicker.failed', { error: catalog.error })}
          </p>
        )}
        {catalog.state === 'loaded' &&
          catalog.machines.map((machine) =>
            row(
              machine.name,
              chosen === machine.name,
              () => choose(machine.name),
              machine.displayName,
              t('composer.machinePicker.size', {
                cpus: String(machine.cpus),
                memory: String(Math.round(machine.memoryBytes / GIB)),
              }),
            ),
          )}
      </PopoverContent>
    </Popover>
  );
}
