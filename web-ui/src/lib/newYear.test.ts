import { describe, expect, test } from 'bun:test';
import {
  FIREWORKS_IDLE,
  createShow,
  fireworksReducer,
  isNewYearDay,
  showDone,
  stepShow,
} from './newYear';

const NEW_YORK = 'America/New_York';
const TOKYO = 'Asia/Tokyo';

describe('isNewYearDay', () => {
  test('is true on December 31 and January 1 and false on December 30 and January 2', () => {
    for (const zone of [NEW_YORK, TOKYO]) {
      // Local noon of each day, expressed as an instant in that zone.
      const noon = (date: string) => new Date(`${date}T12:00:00${zone === NEW_YORK ? '-05:00' : '+09:00'}`);
      expect(isNewYearDay(noon('2026-12-30'), zone)).toBe(false);
      expect(isNewYearDay(noon('2026-12-31'), zone)).toBe(true);
      expect(isNewYearDay(noon('2027-01-01'), zone)).toBe(true);
      expect(isNewYearDay(noon('2027-01-02'), zone)).toBe(false);
    }
  });

  test('follows the configured zone across the date boundary', () => {
    // 2027-01-01T23:00 in New York is already January 2 in Tokyo.
    const instant = new Date('2027-01-01T23:00:00-05:00');
    expect(isNewYearDay(instant, NEW_YORK)).toBe(true);
    expect(isNewYearDay(instant, TOKYO)).toBe(false);
  });
});

describe('fireworks', () => {
  test('starting and ending early returns to the idle state untouched', () => {
    const started = fireworksReducer(FIREWORKS_IDLE, { type: 'start' });
    expect(started.playing).toBe(true);
    expect(fireworksReducer(started, { type: 'end' })).toEqual(FIREWORKS_IDLE);
    expect(fireworksReducer(FIREWORKS_IDLE, { type: 'end' })).toBe(FIREWORKS_IDLE);
  });

  test('the show fires every burst and ends by itself with no sparks left', () => {
    let seed = 7;
    const random = () => {
      seed = (seed * 16807) % 2147483647;
      return seed / 2147483647;
    };
    let show = createShow(800, 600, random);
    const bursts = show.pending.length;
    let peak = 0;
    while (!showDone(show)) {
      show = stepShow(show, 16, random);
      peak = Math.max(peak, show.sparks.length);
    }
    expect(show.pending).toHaveLength(0);
    expect(peak).toBeGreaterThan(bursts);
  });
});
