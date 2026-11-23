/** Styled span produced by the terminal ANSI SGR + carriage-return renderer. */
export interface AnsiStyle {
  fg?: string;
  bg?: string;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
}

export interface AnsiSpan {
  text: string;
  style: AnsiStyle;
}

const ANSI_16_FG: Record<number, string> = {
  30: '#1e1e1e',
  31: '#dc2626',
  32: '#16a34a',
  33: '#ca8a04',
  34: '#2563eb',
  35: '#9333ea',
  36: '#0891b2',
  37: '#d4d4d8',
  90: '#71717a',
  91: '#ef4444',
  92: '#22c55e',
  93: '#eab308',
  94: '#3b82f6',
  95: '#a855f7',
  96: '#06b6d4',
  97: '#f4f4f5',
};

const ANSI_16_BG: Record<number, string> = {
  40: '#1e1e1e',
  41: '#dc2626',
  42: '#16a34a',
  43: '#ca8a04',
  44: '#2563eb',
  45: '#9333ea',
  46: '#0891b2',
  47: '#d4d4d8',
  100: '#71717a',
  101: '#ef4444',
  102: '#22c55e',
  103: '#eab308',
  104: '#3b82f6',
  105: '#a855f7',
  106: '#06b6d4',
  107: '#f4f4f5',
};

function color256(index: number): string | undefined {
  if (index < 0 || index > 255) return undefined;
  if (index < 8) return ANSI_16_FG[30 + index];
  if (index < 16) return ANSI_16_FG[90 + (index - 8)];
  if (index < 232) {
    const cube = index - 16;
    const r = Math.floor(cube / 36);
    const g = Math.floor((cube % 36) / 6);
    const b = cube % 6;
    const step = (v: number) => (v === 0 ? 0 : 55 + v * 40);
    return `rgb(${step(r)}, ${step(g)}, ${step(b)})`;
  }
  const gray = 8 + (index - 232) * 10;
  return `rgb(${gray}, ${gray}, ${gray})`;
}

function sameStyle(a: AnsiStyle, b: AnsiStyle): boolean {
  return (
    a.fg === b.fg &&
    a.bg === b.bg &&
    Boolean(a.bold) === Boolean(b.bold) &&
    Boolean(a.dim) === Boolean(b.dim) &&
    Boolean(a.italic) === Boolean(b.italic) &&
    Boolean(a.underline) === Boolean(b.underline)
  );
}

function applySgr(params: number[], style: AnsiStyle): AnsiStyle {
  let next: AnsiStyle = { ...style };
  const codes = params.length === 0 ? [0] : params;
  for (let i = 0; i < codes.length; i += 1) {
    const code = codes[i];
    if (code === 0) {
      next = {};
    } else if (code === 1) {
      next.bold = true;
    } else if (code === 2) {
      next.dim = true;
    } else if (code === 3) {
      next.italic = true;
    } else if (code === 4) {
      next.underline = true;
    } else if (code === 22) {
      delete next.bold;
      delete next.dim;
    } else if (code === 23) {
      delete next.italic;
    } else if (code === 24) {
      delete next.underline;
    } else if (code === 39) {
      delete next.fg;
    } else if (code === 49) {
      delete next.bg;
    } else if (code in ANSI_16_FG) {
      next.fg = ANSI_16_FG[code];
    } else if (code in ANSI_16_BG) {
      next.bg = ANSI_16_BG[code];
    } else if ((code === 38 || code === 48) && i + 1 < codes.length) {
      const mode = codes[i + 1];
      let resolved: string | undefined;
      if (mode === 5 && i + 2 < codes.length) {
        resolved = color256(codes[i + 2]);
        i += 2;
      } else if (mode === 2 && i + 4 < codes.length) {
        const r = Math.max(0, Math.min(255, codes[i + 2]));
        const g = Math.max(0, Math.min(255, codes[i + 3]));
        const b = Math.max(0, Math.min(255, codes[i + 4]));
        resolved = `rgb(${r}, ${g}, ${b})`;
        i += 4;
      }
      if (resolved) {
        if (code === 38) next.fg = resolved;
        else next.bg = resolved;
      }
    }
  }
  return next;
}

interface Cell {
  ch: string;
  style: AnsiStyle;
}

function cellsToSpans(cells: Cell[]): AnsiSpan[] {
  if (cells.length === 0) return [];
  const spans: AnsiSpan[] = [];
  let currentStyle = cells[0].style;
  let currentText = cells[0].ch;
  for (let i = 1; i < cells.length; i += 1) {
    const cell = cells[i];
    if (sameStyle(currentStyle, cell.style)) {
      currentText += cell.ch;
    } else {
      spans.push({ text: currentText, style: currentStyle });
      currentStyle = cell.style;
      currentText = cell.ch;
    }
  }
  spans.push({ text: currentText, style: currentStyle });
  return spans;
}

/**
 * Parses terminal text into styled lines, preserving SGR colors/weights and
 * simulating `\r` carriage-return overwrites and `\x1b[K` erase-in-line.
 */
export function parseAnsiLines(input: string): AnsiSpan[][] {
  if (input.length === 0) return [];
  const lines: AnsiSpan[][] = [];
  let row: Cell[] = [];
  let cursor = 0;
  let style: AnsiStyle = {};

  let i = 0;
  while (i < input.length) {
    const code = input.charCodeAt(i);
    if (code === 0x1b) {
      const next = input[i + 1];
      if (next === '[') {
        let j = i + 2;
        while (j < input.length) {
          const c = input.charCodeAt(j);
          if (c >= 0x40 && c <= 0x7e) break;
          j += 1;
        }
        if (j >= input.length) break;
        const finalByte = input[j];
        const rawParams = input.slice(i + 2, j);
        const params =
          rawParams.length === 0
            ? []
            : rawParams
                .split(/[;:]/)
                .map((part) => Number.parseInt(part, 10))
                .map((n) => (Number.isNaN(n) ? 0 : n));
        if (finalByte === 'm') {
          style = applySgr(params, style);
        } else if (finalByte === 'K') {
          const mode = params[0] ?? 0;
          if (mode === 0) {
            row.length = Math.min(row.length, cursor);
          } else if (mode === 1) {
            for (let k = 0; k <= Math.min(cursor, row.length - 1); k += 1) {
              row[k] = { ch: ' ', style: {} };
            }
          } else if (mode === 2) {
            row = [];
          }
        } else if (finalByte === 'G') {
          const col = Math.max(1, params[0] ?? 1) - 1;
          cursor = col;
        }
        i = j + 1;
        continue;
      }
      if (next === ']') {
        // OSC sequence terminated by BEL (\x07) or ST (\x1b\\).
        let j = i + 2;
        while (j < input.length) {
          if (input.charCodeAt(j) === 0x07) {
            j += 1;
            break;
          }
          if (input.charCodeAt(j) === 0x1b && input[j + 1] === '\\') {
            j += 2;
            break;
          }
          j += 1;
        }
        i = j;
        continue;
      }
      i += 2;
      continue;
    }

    if (code === 0x0d) {
      if (input.charCodeAt(i + 1) === 0x0a) {
        lines.push(cellsToSpans(row));
        row = [];
        cursor = 0;
        i += 2;
        continue;
      }
      cursor = 0;
      i += 1;
      continue;
    }

    if (code === 0x0a) {
      lines.push(cellsToSpans(row));
      row = [];
      cursor = 0;
      i += 1;
      continue;
    }

    if (code === 0x08) {
      cursor = Math.max(0, cursor - 1);
      i += 1;
      continue;
    }

    const cp = input.codePointAt(i)!;
    const ch = String.fromCodePoint(cp);
    while (row.length < cursor) {
      row.push({ ch: ' ', style: {} });
    }
    row[cursor] = { ch, style };
    cursor += 1;
    i += ch.length;
  }

  if (row.length > 0 || input.endsWith('\n')) {
    if (row.length > 0) {
      lines.push(cellsToSpans(row));
    }
  }
  return lines;
}

/** Whether `text` contains an ANSI escape or standalone carriage return. */
export function hasAnsiOrCarriageReturn(text: string): boolean {
  return text.includes('\x1b') || text.includes('\r');
}
