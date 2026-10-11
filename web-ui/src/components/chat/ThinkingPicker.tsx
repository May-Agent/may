import { useCallback, useState } from 'react';
import { Brain, Check, ChevronDown } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTranslation } from '@/lib/i18n';
import { useSessionsStore } from '@/lib/stores/sessions';
import { cn } from '@/lib/utils';
import type { ReasoningEffortPatch } from '@/lib/api/sessions';

type Effort = 'off' | 'low' | 'medium' | 'high';

interface ThinkingPickerProps {
  sessionId: string;
  current: string | null | undefined;
  disabled?: boolean;
}

function normalize(current: string | null | undefined): Effort {
  const v = current?.toLowerCase();
  if (v === 'low' || v === 'medium' || v === 'high') return v;
  return 'off';
}

function toPatch(effort: Effort): ReasoningEffortPatch {
  return effort === 'off' ? null : effort;
}

export function ThinkingPicker({ sessionId, current, disabled }: ThinkingPickerProps) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const setReasoningEffort = useSessionsStore((s) => s.setReasoningEffort);
  const { t } = useTranslation();
  const active = normalize(current);

  const label = t(`composer.thinkingPicker.${active}`);

  const choose = useCallback(
    async (next: Effort) => {
      if (next === active) {
        setOpen(false);
        return;
      }
      setPending(true);
      try {
        await setReasoningEffort(sessionId, toPatch(next));
      } finally {
        setPending(false);
        setOpen(false);
      }
    },
    [sessionId, active, setReasoningEffort],
  );

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          disabled={disabled}
          className={cn(
            'h-7 gap-1.5 px-2 text-xs font-normal',
            active === 'off' ? 'text-muted-foreground' : 'text-foreground',
          )}
          aria-label={t('composer.thinkingPicker.label')}
        >
          <Brain className="size-3.5" />
          <span className="truncate">{label}</span>
          <ChevronDown className="size-3 opacity-60" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-64 p-1">
        <div className="px-2 pt-1.5 pb-1 text-[11px] uppercase tracking-wider text-muted-foreground">
          {t('composer.thinkingPicker.label')}
        </div>
        <ul className="flex flex-col">
          {(['off', 'low', 'medium', 'high'] as const).map((option) => (
            <li key={option}>
              <button
                type="button"
                disabled={pending}
                onClick={() => void choose(option)}
                className={cn(
                  'flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent disabled:pointer-events-none disabled:opacity-50',
                )}
              >
                <span className="flex-1">{t(`composer.thinkingPicker.${option}`)}</span>
                {option === active && <Check className="size-3.5 text-muted-foreground" />}
              </button>
            </li>
          ))}
        </ul>
        <p className="px-2 pt-1 pb-1 text-[11px] text-muted-foreground/80">
          {t('composer.thinkingPicker.description')}
        </p>
      </PopoverContent>
    </Popover>
  );
}
