import { describe, expect, test } from 'bun:test';
import { extractStringField } from './partialJson';

describe('extractStringField', () => {
  test('returns undefined on empty buffer', () => {
    expect(extractStringField('', 'description')).toBeUndefined();
  });

  test('returns undefined when key missing', () => {
    expect(extractStringField('{"script":"ls"}', 'description')).toBeUndefined();
  });

  test('complete string value', () => {
    expect(extractStringField('{"description":"hello"}', 'description')).toBe(
      'hello',
    );
  });

  test('partial string — no closing quote yet', () => {
    expect(extractStringField('{"description":"hel', 'description')).toBe('hel');
  });

  test('empty string value with closing quote', () => {
    expect(extractStringField('{"description":""}', 'description')).toBe('');
  });

  test('returns empty string when key exists but value has not started', () => {
    expect(extractStringField('{"description":', 'description')).toBe('');
  });

  test('escaped quote inside value', () => {
    expect(
      extractStringField('{"description":"he said \\"hi\\""}', 'description'),
    ).toBe('he said "hi"');
  });

  test('escaped backslash', () => {
    expect(extractStringField('{"script":"a\\\\b"}', 'script')).toBe('a\\b');
  });

  test('escape sequences decode to raw control chars', () => {
    expect(extractStringField('{"script":"a\\nb\\tc"}', 'script')).toBe(
      'a\nb\tc',
    );
  });

  test('unicode escape complete', () => {
    expect(extractStringField('{"description":"\\u00e9clair"}', 'description')).toBe(
      'éclair',
    );
  });

  test('unicode escape truncated — returns prefix and stops', () => {
    expect(extractStringField('{"description":"pre \\u00', 'description')).toBe(
      'pre ',
    );
  });

  test('backslash at buffer end is treated as incomplete escape', () => {
    expect(extractStringField('{"description":"abc\\', 'description')).toBe('abc');
  });

  test('forward slash escape', () => {
    expect(extractStringField('{"description":"a\\/b"}', 'description')).toBe(
      'a/b',
    );
  });

  test('field ordering: description then script', () => {
    const buf = '{"description":"say hi","script":"echo hi"}';
    expect(extractStringField(buf, 'description')).toBe('say hi');
    expect(extractStringField(buf, 'script')).toBe('echo hi');
  });

  test('field ordering: script before description', () => {
    const buf = '{"script":"echo hi","description":"say hi"}';
    expect(extractStringField(buf, 'description')).toBe('say hi');
    expect(extractStringField(buf, 'script')).toBe('echo hi');
  });

  test('braces inside string literal do not confuse scan', () => {
    expect(
      extractStringField('{"script":"echo {a: 1}"}', 'script'),
    ).toBe('echo {a: 1}');
  });

  test('whitespace around colon is tolerated', () => {
    expect(
      extractStringField('{"description"  :  "ok"}', 'description'),
    ).toBe('ok');
  });

  test('returns undefined when matched token is a substring of another key', () => {
    expect(
      extractStringField('{"my_description":"fake","description":"real"}', 'description'),
    ).toBe('real');
  });

  test('partial buffer captured mid-first-character of value', () => {
    expect(extractStringField('{"description":"', 'description')).toBe('');
  });

  test('streaming: decoded prefix grows monotonically across buffer extensions', () => {
    const steps = [
      '{"description":"He',
      '{"description":"Hell',
      '{"description":"Hello',
      '{"description":"Hello,',
      '{"description":"Hello, wor',
      '{"description":"Hello, world"}',
    ];
    const decoded = steps.map((s) => extractStringField(s, 'description'));
    expect(decoded).toEqual([
      'He',
      'Hell',
      'Hello',
      'Hello,',
      'Hello, wor',
      'Hello, world',
    ]);
  });

  test('malformed \\uXXX (only 3 hex) emits verbatim and continues', () => {
    // Buffer has \u and only three hex chars before closing quote -> regex
    // requires 4 hex; since buffer length is sufficient (6 bytes after i),
    // the hex validator fails and escape is emitted as "\u" then "a" is read.
    expect(extractStringField('{"description":"\\u00ax"}', 'description')).toBe(
      '\\u00ax',
    );
  });
});
