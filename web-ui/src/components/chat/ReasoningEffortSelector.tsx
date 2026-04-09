import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

interface ReasoningEffortSelectorProps {
  value?: string;
  onChange: (effort?: string) => void;
  disabled?: boolean;
}

export function ReasoningEffortSelector({
  value,
  onChange,
  disabled = false,
}: ReasoningEffortSelectorProps) {
  return (
    <Select
      value={value ?? 'off'}
      onValueChange={(nextValue) => onChange(nextValue === 'off' ? undefined : nextValue)}
      disabled={disabled}
    >
      <SelectTrigger className="h-8 w-[92px] rounded-lg border-0 bg-transparent px-2 text-xs font-medium text-muted-foreground shadow-none hover:bg-muted/70 hover:text-foreground focus:ring-0">
        <SelectValue />
      </SelectTrigger>
      <SelectContent align="end">
        <SelectItem value="off">Off</SelectItem>
        <SelectItem value="low">Low</SelectItem>
        <SelectItem value="medium">Medium</SelectItem>
        <SelectItem value="high">High</SelectItem>
      </SelectContent>
    </Select>
  );
}
