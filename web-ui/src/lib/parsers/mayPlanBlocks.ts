/**
 * Parser for inline <may-plan> blocks emitted by the model in plan mode.
 *
 * Design goals:
 * - Streaming-safe: partial / unclosed blocks are returned as `open: true`
 *   plan segments so the UI can render a "Drafting plan..." placeholder.
 * - Fence-aware: a `</may-plan>` appearing inside a fenced code block
 *   (```lang ... ```) must be treated as literal content, not as the
 *   closing tag. Same rule protects against fake OPEN tags in code
 *   samples surrounding a plan.
 * - Attribute-aware: supports optional `status="draft|progress|blocked"`
 *   on the opening tag; default is `draft`.
 */

export type MayPlanStatus = 'draft' | 'progress' | 'blocked';

export interface MayPlanSegment {
  kind: 'plan';
  content: string;
  status: MayPlanStatus;
  open: boolean;
  start: number;
  end: number;
}

export interface MayTextSegment {
  kind: 'text';
  content: string;
  start: number;
  end: number;
}

export type MaySegment = MayTextSegment | MayPlanSegment;

const OPEN_TAG_PREFIX = '<may-plan';
const CLOSE_TAG = '</may-plan>';

function isStatus(value: string): value is MayPlanStatus {
  return value === 'draft' || value === 'progress' || value === 'blocked';
}

function parseStatus(tagBody: string): MayPlanStatus {
  const match = /status\s*=\s*["']?([a-zA-Z]+)["']?/.exec(tagBody);
  if (!match) return 'draft';
  const raw = match[1].toLowerCase();
  return isStatus(raw) ? raw : 'draft';
}

/** Triple-backtick fence detection at a given cursor position.
 *  A fence toggle lives at the start of a line (optionally after whitespace)
 *  and spans exactly the three backticks themselves.
 */
function isFenceBoundary(text: string, pos: number): boolean {
  if (text.charCodeAt(pos) !== 0x60) return false;
  if (
    text.charCodeAt(pos + 1) !== 0x60 ||
    text.charCodeAt(pos + 2) !== 0x60
  ) {
    return false;
  }
  let back = pos - 1;
  while (back >= 0) {
    const ch = text.charCodeAt(back);
    if (ch === 0x0a) return true;
    if (ch === 0x20 || ch === 0x09) {
      back--;
      continue;
    }
    return false;
  }
  return true;
}

/** Find the next <may-plan ...> opening tag outside fenced code blocks.
 *  Returns the byte offsets and parsed status, or null if none. */
function findOpenTag(
  text: string,
  from: number,
): { tagStart: number; bodyStart: number; status: MayPlanStatus } | null {
  let cursor = from;
  let inFence = false;
  while (cursor < text.length) {
    if (isFenceBoundary(text, cursor)) {
      inFence = !inFence;
      cursor += 3;
      continue;
    }
    if (!inFence && text.startsWith(OPEN_TAG_PREFIX, cursor)) {
      const afterPrefix = cursor + OPEN_TAG_PREFIX.length;
      const nextCh = text.charCodeAt(afterPrefix);
      // Require the char after `<may-plan` to be `>`, whitespace, or `/`
      // so we don't match `<may-planx...`.
      const isBoundary =
        nextCh === 0x3e ||
        nextCh === 0x20 ||
        nextCh === 0x09 ||
        nextCh === 0x0a ||
        nextCh === 0x0d ||
        nextCh === 0x2f ||
        Number.isNaN(nextCh);
      if (!isBoundary) {
        cursor++;
        continue;
      }
      const gt = text.indexOf('>', afterPrefix);
      if (gt < 0) {
        // Streaming: the tag has not finished yet.
        return {
          tagStart: cursor,
          bodyStart: text.length,
          status: 'draft',
        };
      }
      const tagBody = text.slice(afterPrefix, gt);
      return {
        tagStart: cursor,
        bodyStart: gt + 1,
        status: parseStatus(tagBody),
      };
    }
    cursor++;
  }
  return null;
}

/** Locate the matching `</may-plan>` from a starting cursor, honoring
 *  fenced code blocks inside the plan body. */
function findCloseTag(
  text: string,
  from: number,
): { contentEnd: number; blockEnd: number } | null {
  let cursor = from;
  let inFence = false;
  while (cursor < text.length) {
    if (isFenceBoundary(text, cursor)) {
      inFence = !inFence;
      cursor += 3;
      continue;
    }
    if (!inFence && text.startsWith(CLOSE_TAG, cursor)) {
      return {
        contentEnd: cursor,
        blockEnd: cursor + CLOSE_TAG.length,
      };
    }
    cursor++;
  }
  return null;
}

/** Split `text` into an ordered list of text and plan segments.
 *  Unclosed plans (streaming) come back as `open: true`. */
export function extractMayPlanBlocks(text: string): MaySegment[] {
  const segments: MaySegment[] = [];
  let textStart = 0;
  let cursor = 0;

  const flushText = (end: number) => {
    if (end <= textStart) return;
    const content = text.slice(textStart, end);
    if (content.length === 0) return;
    segments.push({ kind: 'text', content, start: textStart, end });
  };

  while (cursor < text.length) {
    const open = findOpenTag(text, cursor);
    if (!open) {
      flushText(text.length);
      return segments;
    }
    flushText(open.tagStart);
    if (open.bodyStart >= text.length) {
      segments.push({
        kind: 'plan',
        content: '',
        status: open.status,
        open: true,
        start: open.tagStart,
        end: text.length,
      });
      return segments;
    }
    const close = findCloseTag(text, open.bodyStart);
    if (!close) {
      segments.push({
        kind: 'plan',
        content: text.slice(open.bodyStart, text.length),
        status: open.status,
        open: true,
        start: open.tagStart,
        end: text.length,
      });
      return segments;
    }
    segments.push({
      kind: 'plan',
      content: text.slice(open.bodyStart, close.contentEnd),
      status: open.status,
      open: false,
      start: open.tagStart,
      end: close.blockEnd,
    });
    cursor = close.blockEnd;
    textStart = cursor;
  }

  flushText(text.length);
  return segments;
}

/** Return true when `text` contains at least one (possibly unclosed)
 *  <may-plan> block. Useful for early routing / layout decisions. */
export function containsMayPlan(text: string): boolean {
  for (const segment of extractMayPlanBlocks(text)) {
    if (segment.kind === 'plan') return true;
  }
  return false;
}
