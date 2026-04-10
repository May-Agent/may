import type { WebFetchTimelineDisplayItem } from '@/lib/timeline';

export function WebFetchTimeline({ item }: { item: WebFetchTimelineDisplayItem }) {
  return (
    <div className="space-y-2">
      {item.url && (
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 font-mono text-[11px] text-foreground break-all">
          {item.url}
        </div>
      )}
      {!item.url && (
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 text-[11px] text-muted-foreground">
          Page content not available
        </div>
      )}
    </div>
  );
}
