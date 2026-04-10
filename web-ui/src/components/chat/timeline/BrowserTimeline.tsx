import { useState } from 'react';
import type { BrowserTimelineDisplayItem } from '@/lib/timeline';

function ScreenshotImage({ src, alt }: { src: string; alt: string }) {
  const [failed, setFailed] = useState(false);
  if (failed) {
    return null;
  }
  return (
    <img
      src={src}
      alt={alt}
      className="h-auto max-h-[420px] w-full max-w-full rounded-lg border border-border/70 bg-muted/20 object-contain"
      loading="lazy"
      onError={() => setFailed(true)}
    />
  );
}

export function BrowserTimeline({ item }: { item: BrowserTimelineDisplayItem }) {
  if (item.result?.page) {
    const { page } = item.result;
    return (
      <div className="space-y-2">
        {page.screenshot?.apiPath ? (
          <div className="space-y-1 rounded-xl border border-border/70 bg-background/70 p-2">
            <ScreenshotImage
              src={page.screenshot.apiPath}
              alt={page.title || page.url}
            />
            <div className="break-all font-mono text-[10px] text-muted-foreground">
              {page.screenshot.artifactPath}
            </div>
          </div>
        ) : null}
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2">
          <div className="text-[13px] font-medium text-foreground">
            {page.title || page.url}
          </div>
          <div className="mt-1 break-all font-mono text-[11px] text-muted-foreground">
            {page.url}
          </div>
          {page.mainHeading ? (
            <div className="mt-2 text-[12px] text-foreground">
              {page.mainHeading}
            </div>
          ) : null}
          {page.textExcerpt ? (
            <div className="mt-2 whitespace-pre-wrap text-[12px] leading-relaxed text-foreground">
              {page.textExcerpt}
            </div>
          ) : null}
        </div>
        <div className="grid gap-2 md:grid-cols-2">
          <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2">
            <div className="text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
              Accessibility
            </div>
            <div className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-[12px] text-foreground">
              <div>Total {page.accessibility.totalNodes}</div>
              <div>Interesting {page.accessibility.interestingNodes}</div>
              <div>Interactive {page.accessibility.interactiveNodes}</div>
              <div>Headings {page.accessibility.headingNodes}</div>
              <div>Landmarks {page.accessibility.landmarkNodes}</div>
            </div>
          </div>
          <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2">
            <div className="text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
              Headings
            </div>
            {page.headings.length > 0 ? (
              <div className="mt-2 space-y-1 text-[12px] text-foreground">
                {page.headings.slice(0, 6).map((heading, headingIndex) => (
                  <div key={`${item.id}-heading-${headingIndex}`} className="line-clamp-2">
                    {heading}
                  </div>
                ))}
              </div>
            ) : (
              <div className="mt-2 text-[12px] text-muted-foreground">
                No headings extracted
              </div>
            )}
          </div>
        </div>
        <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2">
          <div className="text-[11px] uppercase tracking-[0.08em] text-muted-foreground">
            Interactive elements
          </div>
          {page.interactiveElements.length > 0 ? (
            <div className="mt-2 space-y-1.5">
              {page.interactiveElements.slice(0, 8).map((element, elementIndex) => (
                <div
                  key={`${item.id}-interactive-${elementIndex}`}
                  className="rounded-lg border border-border/60 px-2 py-1.5 text-[11px] text-foreground"
                >
                  <span className="font-medium">{element.role}</span>
                  <span className="text-muted-foreground"> · {element.tagName}</span>
                  {element.accessibleName ? (
                    <div className="mt-1">{element.accessibleName}</div>
                  ) : element.text ? (
                    <div className="mt-1">{element.text}</div>
                  ) : null}
                  {element.href ? (
                    <div className="mt-1 break-all font-mono text-[10px] text-muted-foreground">
                      {element.href}
                    </div>
                  ) : null}
                </div>
              ))}
            </div>
          ) : (
            <div className="mt-2 text-[12px] text-muted-foreground">
              No interactive elements extracted
            </div>
          )}
        </div>
      </div>
    );
  }

  if (item.result) {
    return (
      <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 text-[12px] text-foreground">
        <div>Running: {item.result.running ? 'yes' : 'no'}</div>
        {item.result.cdpEndpoint ? (
          <div className="mt-1 break-all font-mono text-[11px] text-muted-foreground">
            {item.result.cdpEndpoint}
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <div className="rounded-xl border border-border/70 bg-background/70 px-3 py-2 text-[11px] text-muted-foreground">
      Browser action completed
    </div>
  );
}
