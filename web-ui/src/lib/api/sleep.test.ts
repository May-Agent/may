import { describe, expect, test } from 'bun:test';
import { applyReminderReport, parseSleepCommand } from './sleep';

describe('applyReminderReport', () => {
  test('the first report only sets the baseline', () => {
    const first = applyReminderReport({ seen: null }, 3);
    expect(first.remind).toBe(false);
    expect(first.baseline.seen).toBe(3);
  });

  test('a growing counter reminds once per step', () => {
    const grown = applyReminderReport({ seen: 3 }, 4);
    expect(grown.remind).toBe(true);
    expect(applyReminderReport(grown.baseline, 4).remind).toBe(false);
  });

  test('a restarted daemon counting from zero does not remind', () => {
    expect(applyReminderReport({ seen: 3 }, 0).remind).toBe(false);
  });
});

describe('parseSleepCommand', () => {
  test('accepts /sleep and /sleep off only', () => {
    expect(parseSleepCommand('/sleep')).toBe('sleep');
    expect(parseSleepCommand('  /sleep off ')).toBe('off');
    expect(parseSleepCommand('/sleep now')).toBeNull();
    expect(parseSleepCommand('go /sleep')).toBeNull();
  });
});
