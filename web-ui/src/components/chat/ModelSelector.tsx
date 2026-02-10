import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Check } from 'lucide-react';
import { MODELS, PROVIDER_ICONS, getModel } from '@/lib/constants';

interface ModelSelectorProps {
  currentModelId: string;
  currentReasoningEffort?: 'low' | 'medium' | 'high';
  onModelChange: (modelId: string) => void;
  onReasoningEffortChange: (effort: 'low' | 'medium' | 'high') => void;
}

export function ModelSelector({ currentModelId, currentReasoningEffort = 'medium', onModelChange, onReasoningEffortChange }: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const selected = getModel(currentModelId);
  const SelectedIcon = PROVIDER_ICONS[selected.provider];

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="sm" className="h-8 gap-2 text-xs text-muted-foreground hover:text-foreground font-medium px-2">
          <SelectedIcon className="h-4 w-4" />
          <span className="truncate">{selected.name}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[300px] p-0" align="start">
        <Command>
          <CommandInput placeholder="Search model..." />
          <CommandList>
            <CommandEmpty>No model found.</CommandEmpty>
            <CommandGroup heading="Available Models">
              {MODELS.map((model) => {
                const Icon = PROVIDER_ICONS[model.provider];
                return (
                  <CommandItem
                    key={model.id}
                    value={model.name}
                    onSelect={() => { onModelChange(model.id); if (!model.reasoning) setOpen(false); }}
                    className="flex items-center gap-2 py-3 cursor-pointer"
                  >
                    <Icon className="h-5 w-5 flex-shrink-0" />
                    <span className="font-medium flex-1">{model.name}</span>
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
              <Select value={currentReasoningEffort} onValueChange={(v: any) => onReasoningEffortChange(v)}>
                <SelectTrigger className="h-7 w-[100px] text-xs"><SelectValue /></SelectTrigger>
                <SelectContent>
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
