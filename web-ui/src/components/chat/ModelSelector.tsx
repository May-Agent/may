import { useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Check, Loader2 } from 'lucide-react';
import { PROVIDER_ICONS, getModel, inferProvider } from '@/lib/constants';
import { useStore } from '@/lib/store';

interface ModelSelectorProps {
  currentModelId: string;
  currentReasoningEffort?: string;
  onModelChange: (modelId: string) => void;
  onReasoningEffortChange: (effort?: string) => void;
}

export function ModelSelector({ currentModelId, currentReasoningEffort, onModelChange, onReasoningEffortChange }: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const { models, modelsLoaded, uiConfig } = useStore();

  const prioritizedModels = useMemo(() => {
    const tiers = uiConfig.fullConfig?.models;
    if (!tiers) return [];

    const orderedIds = [...tiers.advanced, ...tiers.balanced, ...tiers.fast];
    const uniqueOrderedIds = orderedIds.filter((id, i) => orderedIds.indexOf(id) === i);

    return uniqueOrderedIds.map((id) => {
      const existing = models.find((m) => m.id === id);
      return (
        existing ?? {
          id,
          name: id,
          provider: inferProvider(id),
          reasoning: /o1|reason/i.test(id),
        }
      );
    });
  }, [models, uiConfig.fullConfig?.models]);

  const selected = getModel(currentModelId, prioritizedModels);

  if (!modelsLoaded) {
    return (
      <Button variant="ghost" size="sm" className="h-8 gap-2 text-xs text-muted-foreground font-medium px-2" disabled>
        <Loader2 className="h-4 w-4 animate-spin" />
        <span>Loading models…</span>
      </Button>
    );
  }

  if (!selected) {
    return (
      <Button variant="ghost" size="sm" className="h-8 gap-2 text-xs text-muted-foreground font-medium px-2" disabled>
        <span>No model configured</span>
      </Button>
    );
  }

  const SelectedIcon = PROVIDER_ICONS[selected.provider];

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="sm" className="h-8 gap-2 text-xs text-muted-foreground hover:text-foreground font-medium px-2">
          <SelectedIcon className="h-4 w-4" />
          <span className="truncate">{selected.name}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[320px] p-0" align="start">
        <Command>
          <CommandInput placeholder="Search model..." />
          <CommandList>
            <CommandEmpty>No model found.</CommandEmpty>
            <CommandGroup heading="Available Models">
              {prioritizedModels.map((model) => {
                const Icon = PROVIDER_ICONS[model.provider];
                return (
                  <CommandItem
                    key={model.id}
                    value={`${model.name} ${model.id}`}
                    onSelect={() => {
                      onModelChange(model.id);
                      if (!model.reasoning) setOpen(false);
                    }}
                    className="flex items-center gap-2 py-3 cursor-pointer"
                  >
                    <Icon className="h-5 w-5 flex-shrink-0" />
                    <span className="font-medium flex-1 truncate">{model.name}</span>
                    {currentModelId === model.id && <Check className="h-3 w-3 text-primary" />}
                  </CommandItem>
                );
              })}
            </CommandGroup>
          </CommandList>
        </Command>
        {selected.reasoning && (
          <div className="border-t p-3 bg-muted/30">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs font-medium text-muted-foreground">Reasoning Effort</span>
              <Select value={currentReasoningEffort ?? 'off'} onValueChange={(v) => onReasoningEffortChange(v === 'off' ? undefined : v)}>
                <SelectTrigger className="h-7 w-[120px] text-xs"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="off">Off</SelectItem>
                  <SelectItem value="low">Low</SelectItem>
                  <SelectItem value="medium">Medium</SelectItem>
                  <SelectItem value="high">High</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}
