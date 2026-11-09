import { describe, expect, test } from 'bun:test';
import { parseJobPoolSize } from './jobPool';

describe('parseJobPoolSize', () => {
  test('empty text is automatic', () => {
    expect(parseJobPoolSize('')).toBeNull();
    expect(parseJobPoolSize('  ')).toBeNull();
  });

  test('a positive whole number is the size', () => {
    expect(parseJobPoolSize('8')).toBe(8);
    expect(parseJobPoolSize(' 12 ')).toBe(12);
  });

  test('anything else is rejected', () => {
    expect(parseJobPoolSize('0')).toBeUndefined();
    expect(parseJobPoolSize('-3')).toBeUndefined();
    expect(parseJobPoolSize('2.5')).toBeUndefined();
    expect(parseJobPoolSize('many')).toBeUndefined();
  });
});
