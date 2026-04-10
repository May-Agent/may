import type { WebSearchTimelineDisplayItem } from '@/lib/timeline';

export function WebSearchTimeline({ item }: { item: WebSearchTimelineDisplayItem }) {
  return (
    <div className="space-y-2">
      {item.query && (
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 font-mono text-[11px] text-foreground">
          {item.query}
        </div>
      )}
      {item.results.length > 0 ? (
        <div className="space-y-1.5">
          {item.results.map((result, resultIndex) => (
            <a
              key={`${item.id}-result-${resultIndex}`}
              href={result.url}
              target="_blank"
              rel="noreferrer"
              className="block rounded-xl border border-border/70 bg-background/70 px-3 py-2 transition-colors hover:border-foreground/30 hover:bg-background"
            >
              <div className="line-clamp-1 text-[12px] text-foreground">{result.title}</div>
              <div className="text-[11px] text-muted-foreground">{result.host}</div>
              {result.snippet ? (
                <div className="line-clamp-2 text-[11px] text-muted-foreground">{result.snippet}</div>
              ) : null}
            </a>
          ))}
        </div>
      ) : (
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 text-[11px] text-muted-foreground">
          {item.query ? `No results found for "${item.query}"` : 'No results found'}
        </div>
      )}
    </div>
  );
}
