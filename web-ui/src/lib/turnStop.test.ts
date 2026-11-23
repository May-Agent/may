import { describe, expect, test } from 'bun:test';
import { isTurnStop } from './turnStop';

describe('isTurnStop', () => {
  test('a loop the daemon ended is a stop', () => {
    expect(isTurnStop('loop_stopped')).toBeTrue();
  });

  test('any other code is an error', () => {
    expect(isTurnStop('internal_error')).toBeFalse();
    expect(isTurnStop('provider_unavailable')).toBeFalse();
  });
});
