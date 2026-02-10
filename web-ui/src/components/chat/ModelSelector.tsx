import { useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from '@/components/ui/command';
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from '@/components/ui/popover';
import { Check, ChevronsUpDown } from 'lucide-react';
import { OpenAI, Anthropic, Google } from '@/components/icons/providers';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

interface Model {
  id: string;
  name: string;
  provider: 'openai' | 'anthropic' | 'google';
  description: string;
  reasoning?: boolean;
}

const MODELS: Model[] = [
  { 
    id: 'gpt-4', 
    name: 'GPT-4', 
    provider: 'openai', 
    description: 'Most capable model, great for complex tasks' 
  },
  { 
    id: 'o1-preview', 
    name: 'o1-preview', 
    provider: 'openai', 
    description: 'Reasoning model for solving hard problems',
    reasoning: true
  },
  { 
    id: 'gpt-3.5-turbo', 
    name: 'GPT-3.5 Turbo', 
    provider: 'openai', 
    description: 'Fast and cost-effective for everyday tasks' 
  },
  { 
    id: 'claude-3-opus', 
    name: 'Claude 3 Opus', 
    provider: 'anthropic', 
    description: 'Top-tier intelligence, excellent reasoning' 
  },
  { 
    id: 'claude-3-sonnet', 
    name: 'Claude 3 Sonnet', 
    provider: 'anthropic', 
    description: 'Balanced performance and speed' 
  },
  { 
    id: 'gemini-pro', 
    name: 'Gemini Pro', 
    provider: 'google', 
    description: 'Capable multimodal model by Google' 
  },
];

export const getModelDetails = (modelId: string) => {
  const model = MODELS.find(m => m.id === modelId) || MODELS[0];
  const Icon = ({ className }: { className?: string }) => {
    switch (model.provider) {
      case 'openai': return <OpenAI className={className} />;
      case 'anthropic': return <Anthropic className={className} />;
      case 'google': return <Google className={className} />;
      default: return null;
    }
  };
  return { ...model, icon: Icon };
};

const ProviderIcon = ({ provider, className }: { provider: string, className?: string }) => {
  switch (provider) {
    case 'openai': return <OpenAI className={className} />;
    case 'anthropic': return <Anthropic className={className} />;
    case 'google': return <Google className={className} />;
    default: return null;
  }
};

interface ModelSelectorProps {
  currentModelId: string;
  currentReasoningEffort?: 'low' | 'medium' | 'high';
  onModelChange: (modelId: string) => void;
  onReasoningEffortChange: (effort: 'low' | 'medium' | 'high') => void;
  trigger?: React.ReactNode;
}

export function ModelSelector({ 
  currentModelId, 
  currentReasoningEffort = 'medium',
  onModelChange,
  onReasoningEffortChange,
  trigger
}: ModelSelectorProps) {
  const [open, setOpen] = useState(false);
  const selectedModel = MODELS.find(m => m.id === currentModelId) || MODELS[0];

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        {trigger ? (
          trigger
        ) : (
          <Button
            variant="ghost"
            role="combobox"
            aria-expanded={open}
            className="w-auto gap-2 px-2 hover:bg-muted/50 font-semibold text-lg"
          >
            <div className="flex items-center gap-2">
              <span className="truncate">{selectedModel.name}</span>
              <ProviderIcon provider={selectedModel.provider} className="h-4 w-4 flex-shrink-0" />
            </div>
            <ChevronsUpDown className="h-4 w-4 shrink-0 opacity-50" />
          </Button>
        )}
      </PopoverTrigger>
      <PopoverContent className="w-[300px] p-0" align="start">
        <div className="flex flex-col">
          <Command>
            <CommandInput placeholder="Search model..." />
            <CommandList>
              <CommandEmpty>No model found.</CommandEmpty>
              <CommandGroup heading="Available Models">
                {MODELS.map((model) => (
                  <CommandItem
                    key={model.id}
                    value={model.name}
                    onSelect={() => {
                      onModelChange(model.id);
                      if (!model.reasoning) {
                        setOpen(false);
                      }
                    }}
                    className="flex items-start gap-2 py-3 cursor-pointer"
                  >
                    <ProviderIcon provider={model.provider} className="h-5 w-5 mt-0.5 flex-shrink-0" />
                    <div className="flex flex-col gap-1 overflow-hidden">
                      <div className="flex items-center gap-2">
                        <span className="font-medium leading-none">{model.name}</span>
                        {currentModelId === model.id && (
                          <Check className="h-3 w-3 text-primary ml-auto" />
                        )}
                      </div>
                    </div>
                  </CommandItem>
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
          
          {selectedModel.reasoning && (
            <div className="border-t p-3 bg-muted/30">
              <div className="flex items-center justify-between gap-2">
                <span className="text-xs font-medium text-muted-foreground">Reasoning Effort</span>
                <Select
                  value={currentReasoningEffort}
                  onValueChange={(value: 'low' | 'medium' | 'high') => onReasoningEffortChange(value)}
                >
                  <SelectTrigger className="h-7 w-[100px] text-xs">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="low">Low</SelectItem>
                    <SelectItem value="medium">Medium</SelectItem>
                    <SelectItem value="high">High</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
