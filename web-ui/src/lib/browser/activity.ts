import type { TurnToolState } from '../stream/reducer';
import { parsePartialJson } from '../stream/partialJson';

/**
 * What the agent did with the managed browser, read from a `browser`
 * command's arguments and its text result. The command takes
 * `{ command: "open" | "close" | "status" | "cdp-endpoint" | "read", url?,
 * screenshot? }`; a `read` result names the page as `Page: <title> (<url>)`
 * and a capture as `Screenshot saved to <path> (<w>x<h>).`.
 */
export type BrowserActionKind = 'open' | 'close' | 'status' | 'connect' | 'read' | 'other';

export interface BrowserActivity {
  action: BrowserActionKind;
  /** The URL the agent asked for. */
  url: string | null;
  /** Whether the call captures a screenshot. */
  screenshot: boolean;
  status: 'running' | 'done' | 'failed';
  /** The page the browser reported back. */
  pageTitle: string | null;
  pageUrl: string | null;
  /** Session-relative artifact path of the screenshot, e.g. `browser/x.png`. */
  screenshotPath: string | null;
  error: string | null;
}

const PAGE_LINE = /^Page: (.*) \((\S+)\)$/m;
const SHOT_LINE = /^Screenshot saved to (\S+) \(\d+x\d+\)\.$/m;

function resultText(result: unknown): { text: string | null; error: string | null } {
  if (typeof result === 'string') return { text: result, error: null };
  if (!result || typeof result !== 'object') return { text: null, error: null };
  const r = result as Record<string, unknown>;
  if (r.kind === 'error') {
    return { text: null, error: typeof r.message === 'string' ? r.message : 'error' };
  }
  return { text: typeof r.text === 'string' ? r.text : null, error: null };
}

function actionOf(command: unknown): BrowserActionKind {
  switch (command) {
    case 'open':
      return 'open';
    case 'close':
      return 'close';
    case 'status':
      return 'status';
    case 'cdp-endpoint':
      return 'connect';
    case 'read':
      return 'read';
    default:
      return 'other';
  }
}

export function browserActivity(tool: TurnToolState): BrowserActivity {
  const parsed = parsePartialJson(tool.argumentsRaw);
  const args = parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {};
  const action = actionOf(args.command);
  const url =
    typeof args.url === 'string' ? args.url : (tool.summary?.target ?? tool.summary?.url ?? null);
  const screenshot = action === 'read' && args.screenshot !== 'off';
  const { text, error } = resultText(tool.result);
  const page = text ? PAGE_LINE.exec(text) : null;
  const shot = text ? SHOT_LINE.exec(text) : null;
  const shotPath = shot?.[1] ?? null;
  return {
    action,
    url,
    screenshot,
    status: tool.status === 'streaming' ? 'running' : error ? 'failed' : 'done',
    pageTitle: page?.[1]?.trim() || null,
    pageUrl: page?.[2] ?? null,
    // The result names the artifact under the session (`artifacts/…`); the
    // preview API takes the path inside `artifacts/`.
    screenshotPath: shotPath ? shotPath.replace(/^artifacts\//, '') : null,
    error,
  };
}

/** Host name for a compact label, the URL itself if it does not parse. */
export function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/**
 * Consecutive browser calls form one stretch of computer use, shown as one
 * card. Splits an ordered list into runs of browser tools and single items.
 */
export function groupBrowserRuns<T>(
  items: T[],
  isBrowser: (item: T) => boolean,
): Array<{ kind: 'browser'; items: T[] } | { kind: 'item'; item: T }> {
  const out: Array<{ kind: 'browser'; items: T[] } | { kind: 'item'; item: T }> = [];
  for (const item of items) {
    const last = out[out.length - 1];
    if (isBrowser(item)) {
      if (last && last.kind === 'browser') last.items.push(item);
      else out.push({ kind: 'browser', items: [item] });
    } else {
      out.push({ kind: 'item', item });
    }
  }
  return out;
}
