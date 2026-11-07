import { useCallback, useEffect, useState } from 'react';
import { ChevronDown, ChevronRight } from 'lucide-react';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { useTranslation } from '@/lib/i18n';
import { ladderForModel, patchOfEffort, stopOfEffort } from '@/lib/effort';
import { useModelsStore } from '@/lib/stores/models';
import { useSessionsStore } from '@/lib/stores/sessions';
import { EffortSlider } from './EffortSlider';

interface ThinkingPickerProps {
  sessionId: string;
  currentModel: string;
  current: string | null | undefined;
  disabled?: boolean;
}

export function ThinkingPicker({ sessionId, currentModel, current, disabled }: ThinkingPickerProps) {
  const [open, setOpen] = useState(false);
  const [preview, setPreview] = useState<number | null>(null);
  const groups = useModelsStore((s) => s.groups);
  const refresh = useModelsStore((s) => s.refresh);
  const setReasoningEffort = useSessionsStore((s) => s.setReasoningEffort);
  const { t } = useTranslation();

  useEffect(() => {
    if (groups === null) void refresh();
  }, [groups, refresh]);

  const levels = ladderForModel(groups, currentModel);
  const stored = stopOfEffort(levels, current);
  const shown = preview ?? stored;

  const labelOf = useCallback(
    (level: string) => {
      switch (level) {
        case 'none':
          return t('composer.thinkingPicker.none');
        case 'minimal':
          return t('composer.thinkingPicker.minimal');
        case 'low':
          return t('composer.thinkingPicker.low');
        case 'medium':
          return t('composer.thinkingPicker.medium');
        case 'high':
          return t('composer.thinkingPicker.high');
        case 'xhigh':
          return t('composer.thinkingPicker.xhigh');
        case 'max':
          return t('composer.thinkingPicker.max');
        default:
          return level;
      }
    },
    [t],
  );

  const commit = useCallback(
    (stop: number) => {
      setPreview(null);
      const level = levels[stop];
      if (level === undefined || stop === stored) return;
      void setReasoningEffort(sessionId, patchOfEffort(level));
    },
    [levels, stored, sessionId, setReasoningEffort],
  );

  const onOpenChange = (next: boolean) => {
    if (!next && preview !== null) commit(preview);
    setOpen(next);
  };

  if (levels.length === 0) return null;

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          aria-label={t('composer.thinkingPicker.label')}
          className="inline-flex h-7 items-center gap-1 rounded-full bg-muted px-3 text-xs text-foreground transition-colors hover:bg-muted/70 disabled:pointer-events-none disabled:opacity-50"
        >
          <span className="truncate">{labelOf(levels[stored])}</span>
          <ChevronDown className="size-3 opacity-60" />
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="flex w-[min(30rem,calc(100vw-2rem))] flex-col items-center gap-4 rounded-2xl bg-popover px-6 pt-4 pb-5 text-popover-foreground shadow-xl"
      >
        <span className="inline-flex items-center gap-0.5 rounded-full bg-muted px-3 py-1 text-xs font-medium text-muted-foreground">
          {labelOf(levels[shown])}
          <ChevronRight className="size-3 opacity-60" />
        </span>
        <EffortSlider
          levels={levels}
          value={shown}
          labelOf={labelOf}
          onChange={setPreview}
          onCommit={commit}
          ariaLabel={t('composer.thinkingPicker.label')}
        />
      </PopoverContent>
    </Popover>
  );
}
