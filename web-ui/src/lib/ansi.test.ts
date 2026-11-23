import { describe, expect, test } from 'bun:test';
import { hasAnsiOrCarriageReturn, parseAnsiLines } from './ansi';

describe('parseAnsiLines', () => {
  test('parses 16-color, 256-color, and 24-bit SGR spans across CJK text', () => {
    const input =
      '\x1b[1;31m编译错误\x1b[0m \x1b[38;5;208m警告です\x1b[0m \x1b[38;2;16;185;129m통과\x1b[0m\nplain';
    expect(hasAnsiOrCarriageReturn(input)).toBe(true);
    const lines = parseAnsiLines(input);
    expect(lines).toHaveLength(2);
    expect(lines[0]).toEqual([
      { text: '编译错误', style: { bold: true, fg: '#dc2626' } },
      { text: ' ', style: {} },
      { text: '警告です', style: { fg: 'rgb(255, 135, 0)' } },
      { text: ' ', style: {} },
      { text: '통과', style: { fg: 'rgb(16, 185, 129)' } },
    ]);
    expect(lines[1]).toEqual([{ text: 'plain', style: {} }]);
  });

  test('simulates carriage-return overwrites and erase-in-line', () => {
    const input = 'Compiling 1/10\rCompiling 10/10\nStep A...\rDone\x1b[K\n';
    const lines = parseAnsiLines(input);
    expect(lines.map((row) => row.map((s) => s.text).join(''))).toEqual([
      'Compiling 10/10',
      'Done',
    ]);
  });
});
