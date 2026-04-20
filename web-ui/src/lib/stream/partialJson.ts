/**
 * Partial JSON parser — recovers a best-effort object from an incomplete JSON
 * fragment. Used to show tool arguments (command, description, url, query…)
 * as they stream in, before the full JSON is closed.
 *
 * The parser attempts to balance quotes/brackets and trim a trailing comma so
 * that `JSON.parse` on the repaired text succeeds. If repair fails at any
 * point, it returns null and the caller falls back to the raw fragment.
 */

type Char = string;

const PAIRS: Record<Char, Char> = {
  '{': '}',
  '[': ']',
};

interface RepairState {
  closing: Char[];
  inString: boolean;
  lastSignificant: Char;
  escapeNext: boolean;
}

function repair(fragment: string): string | null {
  const state: RepairState = {
    closing: [],
    inString: false,
    lastSignificant: '',
    escapeNext: false,
  };

  for (let i = 0; i < fragment.length; i++) {
    const ch = fragment[i];
    if (ch === undefined) break;

    if (state.inString) {
      if (state.escapeNext) {
        state.escapeNext = false;
        continue;
      }
      if (ch === '\\') {
        state.escapeNext = true;
        continue;
      }
      if (ch === '"') {
        state.inString = false;
        state.lastSignificant = ch;
      }
      continue;
    }

    if (ch === '"') {
      state.inString = true;
      state.lastSignificant = ch;
      continue;
    }

    if (ch === '{' || ch === '[') {
      state.closing.push(PAIRS[ch]!);
      state.lastSignificant = ch;
      continue;
    }

    if (ch === '}' || ch === ']') {
      const expected = state.closing.pop();
      if (expected !== ch) return null;
      state.lastSignificant = ch;
      continue;
    }

    if (ch !== ' ' && ch !== '\n' && ch !== '\t' && ch !== '\r') {
      state.lastSignificant = ch;
    }
  }

  let repaired = fragment;
  if (state.inString) {
    if (state.escapeNext) repaired = repaired.slice(0, -1);
    repaired += '"';
  }

  if (state.lastSignificant === ',') {
    const idx = repaired.lastIndexOf(',');
    if (idx >= 0) repaired = repaired.slice(0, idx) + repaired.slice(idx + 1);
  }

  if (state.lastSignificant === ':') {
    repaired += 'null';
  }

  while (state.closing.length > 0) {
    repaired += state.closing.pop();
  }

  return repaired;
}

/**
 * Parse a (possibly incomplete) JSON string. Returns the parsed value or null
 * if repair fails. Empty/whitespace input returns an empty object.
 */
export function parsePartialJson(fragment: string): unknown {
  const trimmed = fragment.trim();
  if (trimmed.length === 0) return {};

  try {
    return JSON.parse(trimmed);
  } catch {
    // fall through to repair
  }

  const repaired = repair(trimmed);
  if (repaired === null) return null;

  try {
    return JSON.parse(repaired);
  } catch {
    return null;
  }
}

/**
 * Project a specific string field out of a streaming JSON fragment. Handy for
 * rendering the live value of `command`, `description`, `url`, `query`, etc.
 * as they stream in. Returns '' if the field is not yet present.
 */
export function projectField(fragment: string, field: string): string {
  const parsed = parsePartialJson(fragment);
  if (parsed === null || typeof parsed !== 'object') return '';
  const record = parsed as Record<string, unknown>;
  const value = record[field];
  return typeof value === 'string' ? value : '';
}
