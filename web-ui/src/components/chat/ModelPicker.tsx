import { useCallback, useEffect, useMemo, useState } from 'react';
import { Check, ChevronDown, Cpu } from 'lucide-react';
import { Command } from 'cmdk';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTranslation } from '@/lib/i18n';
import { listModelsByProvider, type ProviderModelsGroup } from '@/lib/api/models';
import { useSessionsStore } from '@/lib/stores/sessions';
import { cn } from '@/lib/utils';

interface ModelPickerProps {
  sessionId: string;
  currentModel: string;
  disabled?: boolean;
}

export function ModelPicker({ sessionId, currentModel, disabled }: ModelPickerProps) {
  const [open, setOpen] = useState(false);
  const [groups, setGroups] = useState<ProviderModelsGroup[] | null>(null);
  const [pending, setPending] = useState(false);
  const setModel = useSessionsStore((s) => s.setModel);
  const { t } = useTranslation();

  useEffect(() => {
    if (!open || groups) return;
    void (async () => {
      try {
        const res = await listModelsByProvider();
        setGroups(res.providers);
      } catch {
        setGroups([]);
      }
    })();
  }, [open, groups]);

  const selectorLabel = (selector: string) => {
    const [, modelId] = selector.split('::', 2);
    return modelId ?? selector;
  };

  const displayLabel = useMemo(() => {
    if (!groups) return selectorLabel(currentModel);
    for (const group of groups) {
      const hit = group.details.find((d) => d.id === currentModel);
      if (hit) return hit.nickname?.trim() || hit.modelId;
    }
    return selectorLabel(currentModel);
  }, [groups, currentModel]);

  const handleSelect = useCallback(
    async (modelId: string) => {
      if (modelId === currentModel) {
        setOpen(false);
        return;
      }
      setPending(true);
      try {
        await setModel(sessionId, modelId);
      } finally {
        setPending(false);
        setOpen(false);
      }
    },
    [sessionId, currentModel, setModel],
  );

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={disabled}
          className="h-7 gap-1.5 px-2 text-xs font-normal text-muted-foreground hover:text-foreground"
          aria-label={t('composer.modelPicker.label')}
        >
          <Cpu className="size-3.5" />
          <span className="max-w-[140px] truncate">{displayLabel}</span>
          <ChevronDown className="size-3 opacity-60" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-80 p-0">
        <Command className="flex flex-col overflow-hidden" loop>
          <Command.Input
            placeholder={t('composer.modelPicker.searchPlaceholder')}
            className="w-full border-b border-border bg-transparent px-3 py-2 text-sm outline-none placeholder:text-muted-foreground"
          />
          <Command.List className="max-h-[320px] overflow-y-auto p-1 scrollbar-thin">
            <Command.Empty className="px-3 py-6 text-center text-xs text-muted-foreground">
              {t('composer.modelPicker.empty')}
            </Command.Empty>
            {groups?.map((group) => (
              <Command.Group
                key={group.providerId}
                heading={group.providerId}
                className="text-[11px] uppercase tracking-wider text-muted-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2"
              >
                {group.details.map((detail) => {
                  const label = detail.nickname?.trim() || detail.modelId;
                  const active = detail.id === currentModel;
                  return (
                    <Command.Item
                      key={detail.id}
                      value={`${detail.id} ${detail.modelId} ${label}`}
                      onSelect={() => void handleSelect(detail.id)}
                      className={cn(
                        'flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-sm aria-selected:bg-accent',
                        pending && 'pointer-events-none opacity-50',
                      )}
                    >
                      <span className="flex-1 truncate">{label}</span>
                      {detail.tier && (
                        <span className="text-[10px] uppercase tracking-wider text-muted-foreground/70">
                          {detail.tier}
                        </span>
                      )}
                      {active && <Check className="size-3.5 text-muted-foreground" />}
                    </Command.Item>
                  );
                })}
              </Command.Group>
            ))}
          </Command.List>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
