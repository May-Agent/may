import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Check } from 'lucide-react';
import { PROVIDER_ICONS, getModel } from '@/lib/constants';
import { useStore } from '@/lib/store';

interface ModelSelectorProps {
  currentModelId: string;
  currentReasoningEffort?: string;
  onModelChange: (modelId: string) => void;
  onReasoningEffortChange: (effort: string) => void;
}

export function ModelSelector({ currentModelId, currentReasoningEffort, onModelChange, onReasoningEffortChange }: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const { models } = useStore();
  const selected = getModel(currentModelId, models);
  const SelectedIcon = PROVIDER_ICONS[selected.provider];
  const alwaysReasoning = !!selected.alwaysReasoning;
  const availableEfforts = (selected.reasoningEfforts?.length ? selected.reasoningEfforts : ['low', 'medium', 'high'])
    .filter((e) => !alwaysReasoning || e === 'low');
  const selectedEffort = alwaysReasoning
    ? 'low'
    : (currentReasoningEffort || selected.defaultReasoningEffort || availableEfforts[0] || 'medium');

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
              {models.map((model) => {
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
        {selected.reasoning && selected.kind !== 'image' && selected.kind !== 'embedding' && selected.kind !== 'reranker' && (
          <div className="border-t p-3 bg-muted/30">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs font-medium text-muted-foreground">Reasoning Effort</span>
              <Select
                value={selectedEffort}
                onValueChange={(v) => onReasoningEffortChange(v)}
                disabled={alwaysReasoning}
              >
                <SelectTrigger className="h-7 w-[120px] text-xs"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {availableEfforts.map((effort) => (
                    <SelectItem key={effort} value={effort}>{effort.toUpperCase()}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            {alwaysReasoning && (
              <div className="mt-1 text-[11px] text-muted-foreground">This model always uses reasoning (low).</div>
            )}
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}
