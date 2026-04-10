/**
 * Skeleton placeholder shown while a profile's sessions are being
 * fetched. Three stacked pulsing blocks roughly match the shape of a
 * chat transcript so the layout doesn't jump when content arrives.
 */
export function ProfileLoadingScreen() {
  return (
    <div className="flex-1 min-h-0 px-4 pt-10">
      <div className="mx-auto w-full max-w-3xl space-y-4">
        <div className="h-4 w-40 rounded-full bg-muted/70 animate-pulse" />
        <div className="space-y-3 rounded-2xl border border-border/50 bg-background/70 p-4">
          <div className="h-3 w-3/4 rounded-full bg-muted/60 animate-pulse" />
          <div className="h-3 w-5/6 rounded-full bg-muted/50 animate-pulse [animation-delay:120ms]" />
          <div className="h-3 w-2/3 rounded-full bg-muted/40 animate-pulse [animation-delay:240ms]" />
        </div>
        <div className="space-y-3 rounded-2xl border border-border/40 bg-background/60 p-4">
          <div className="h-3 w-2/3 rounded-full bg-muted/60 animate-pulse [animation-delay:160ms]" />
          <div className="h-3 w-3/4 rounded-full bg-muted/50 animate-pulse [animation-delay:280ms]" />
        </div>
      </div>
    </div>
  );
}
