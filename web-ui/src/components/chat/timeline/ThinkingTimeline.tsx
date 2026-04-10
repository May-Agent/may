import type { ThinkingTimelineDisplayItem } from '@/lib/timeline';

export function ThinkingTimeline({ item }: { item: ThinkingTimelineDisplayItem }) {
  return (
    <pre className="max-h-[240px] overflow-y-auto whitespace-pre-wrap rounded-xl border border-border/70 bg-background/70 p-3 font-mono text-[11px] leading-relaxed text-foreground scrollbar-none">
      {item.detail}
    </pre>
  );
}
