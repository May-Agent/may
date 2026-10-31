import { describe, expect, test } from 'bun:test';
import { formatGroupTime, startsGroup } from './time';

describe('startsGroup', () => {
  test('the first message and a long gap start a group', () => {
    expect(startsGroup(undefined, '2026-10-31T09:00:00Z')).toBe(true);
    expect(startsGroup('2026-10-31T09:00:00Z', '2026-10-31T09:20:00Z')).toBe(true);
  });
  test('close messages stay together; bad timestamps add nothing', () => {
    expect(startsGroup('2026-10-31T09:00:00Z', '2026-10-31T09:05:00Z')).toBe(false);
    expect(startsGroup('2026-10-31T09:00:00Z', 'nope')).toBe(false);
    expect(startsGroup('nope', '2026-10-31T09:00:00Z')).toBe(true);
  });
});

describe('formatGroupTime', () => {
  const now = new Date(2026, 9, 31, 15, 0);
  const local = (d: Date) => d.toISOString();
  test('today shows the time only', () => {
    const s = formatGroupTime(local(new Date(2026, 9, 31, 9, 12)), now, 'en-US');
    expect(s).toBe('9:12 AM');
  });
  test('this week adds the weekday, older adds the date', () => {
    expect(formatGroupTime(local(new Date(2026, 9, 28, 9, 12)), now, 'en-US')).toBe('Wed 9:12 AM');
    expect(formatGroupTime(local(new Date(2026, 9, 3, 9, 12)), now, 'en-US')).toBe('Oct 3, 9:12 AM');
  });
});
