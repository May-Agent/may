import { describe, expect, test } from 'bun:test';
import { parsePartialJson, projectField } from './partialJson';

describe('parsePartialJson', () => {
  test('parses complete JSON unchanged', () => {
    expect(parsePartialJson('{"command":"ls","description":"list"}')).toEqual({
      command: 'ls',
      description: 'list',
    });
  });

  test('handles empty input', () => {
    expect(parsePartialJson('')).toEqual({});
    expect(parsePartialJson('   ')).toEqual({});
  });

  test('recovers from unterminated string', () => {
    expect(parsePartialJson('{"command":"ls -')).toEqual({ command: 'ls -' });
  });

  test('recovers from missing closing brace', () => {
    expect(parsePartialJson('{"command":"ls","description":"listing"')).toEqual({
      command: 'ls',
      description: 'listing',
    });
  });

  test('recovers from trailing comma', () => {
    expect(parsePartialJson('{"command":"ls",')).toEqual({ command: 'ls' });
  });

  test('drops incomplete key on trailing colon', () => {
    expect(parsePartialJson('{"command":')).toEqual({});
  });

  test('handles nested objects mid-stream', () => {
    expect(parsePartialJson('{"env":{"PATH":"/usr')).toEqual({ env: { PATH: '/usr' } });
  });

  test('handles nested arrays mid-stream', () => {
    expect(parsePartialJson('{"args":["--help"')).toEqual({ args: ['--help'] });
  });

  test('preserves escaped quote inside string', () => {
    expect(parsePartialJson('{"command":"echo \\"hello')).toEqual({
      command: 'echo "hello',
    });
  });

  test('trailing backslash in string is dropped so repair parses', () => {
    expect(parsePartialJson('{"command":"echo \\')).toEqual({ command: 'echo ' });
  });

  test('returns null for structurally invalid fragment', () => {
    expect(parsePartialJson('}{')).toBeNull();
  });
});

describe('projectField', () => {
  test('returns field from streaming fragment char-by-char', () => {
    const input = '{"command":"npm run build","description":"build prod"}';
    const progression: string[] = [];
    for (let i = 1; i <= input.length; i++) {
      progression.push(projectField(input.slice(0, i), 'command'));
    }
    // The field value should grow monotonically once the key appears.
    const firstNonEmpty = progression.findIndex((v) => v.length > 0);
    expect(firstNonEmpty).toBeGreaterThan(0);
    for (let i = firstNonEmpty + 1; i < progression.length; i++) {
      const prev = progression[i - 1]!;
      const curr = progression[i]!;
      // Monotonic until the closing quote is reached, after which value stays.
      expect(curr.startsWith(prev) || prev.startsWith(curr)).toBe(true);
    }
    expect(progression[progression.length - 1]).toBe('npm run build');
  });

  test('renders description streamingly after command closes', () => {
    const input = '{"command":"ls","description":"listing files"}';
    const frames: string[] = [];
    for (let i = 1; i <= input.length; i++) {
      frames.push(projectField(input.slice(0, i), 'description'));
    }
    expect(frames[frames.length - 1]).toBe('listing files');
    expect(frames.some((f) => f === 'listing')).toBe(true);
  });

  test('streams multiline script content before JSON closes', () => {
    const script = "python3 - <<'PY'\nfor i in range(3):\n    print(i)\nPY";
    const input = JSON.stringify({ script });
    const frames: string[] = [];
    for (let i = 1; i <= input.length; i++) {
      frames.push(projectField(input.slice(0, i), 'script'));
    }

    expect(frames.some((f) => f.includes('for i in range'))).toBe(true);
    expect(frames.some((f) => f.includes('    print'))).toBe(true);
    expect(frames[frames.length - 1]).toBe(script);
  });

  test('returns empty string when field absent', () => {
    expect(projectField('{"other":"x"}', 'command')).toBe('');
  });

  test('returns empty string for non-string field value', () => {
    expect(projectField('{"command":42}', 'command')).toBe('');
  });
});
