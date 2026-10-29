/** Typed command record carried by a terminal tool result (`kind: "command"`). */

export interface OutputStream {
  text: string;
  truncated: boolean;
  /**
   * Relative outputs/* path when the full payload is persisted to disk.
   * Truncation in itself is an internal context-management concern and is
   * never surfaced to the user as a warning; we only expose it indirectly
   * by offering a link to the full file when one exists.
   */
  url: string | null;
}

export interface TerminalResult {
  exitCode: number | null;
  stdout: OutputStream | null;
  stderr: OutputStream | null;
}

function extractOutputStream(entry: unknown): OutputStream | null {
  if (!entry || typeof entry !== 'object') return null;
  const e = entry as Record<string, unknown>;
  const file =
    e.file && typeof e.file === 'object' ? (e.file as Record<string, unknown>) : null;
  const url = file && typeof file.url === 'string' && file.url.length > 0 ? file.url : null;
  switch (e.kind) {
    case 'inline':
      if (typeof e.text !== 'string' || e.text.length === 0) return null;
      return { text: e.text, truncated: false, url: null };
    case 'stored':
      if (typeof e.preview !== 'string' || e.preview.length === 0) return null;
      return { text: e.preview, truncated: e.truncated === true, url };
    case 'image':
      return {
        text: `[image${typeof e.media_type === 'string' ? `: ${e.media_type}` : ''}]`,
        truncated: false,
        url,
      };
    default:
      return null;
  }
}

export function extractTerminalResult(result: unknown): TerminalResult | null {
  if (!result || typeof result !== 'object') return null;
  const r = result as Record<string, unknown>;
  if (r.kind !== 'command') return null;
  const outcome =
    r.outcome && typeof r.outcome === 'object'
      ? (r.outcome as Record<string, unknown>)
      : null;
  return {
    exitCode:
      outcome && outcome.status === 'exited' && typeof outcome.code === 'number'
        ? outcome.code
        : null,
    stdout: extractOutputStream(r.stdout),
    stderr: extractOutputStream(r.stderr),
  };
}
