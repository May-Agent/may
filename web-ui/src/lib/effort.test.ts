import { describe, expect, test } from 'bun:test';
import type { ProviderModelsGroup } from './api/models';
import {
  ladderForModel,
  nearestStop,
  patchOfEffort,
  stopAfterKey,
  stopFraction,
  stopOfEffort,
} from './effort';

const claude = ['none', 'low', 'medium', 'high', 'xhigh', 'max'];

const groups: ProviderModelsGroup[] = [
  {
    providerId: 'relay',
    models: ['relay::reasoner', 'relay::plain'],
    details: [
      {
        id: 'relay::reasoner',
        modelId: 'reasoner',
        providerId: 'relay',
        nickname: 'reasoner',
        effortLadder: claude,
      },
      {
        id: 'relay::plain',
        modelId: 'plain',
        providerId: 'relay',
        nickname: 'plain',
        effortLadder: [],
      },
    ],
  },
];

describe('effort ladder', () => {
  test('follows the selected model and is empty without effort control', () => {
    expect(ladderForModel(groups, 'relay::reasoner')).toEqual(claude);
    expect(ladderForModel(groups, 'relay::plain')).toEqual([]);
    expect(ladderForModel(groups, 'relay::unknown')).toEqual([]);
    expect(ladderForModel(null, 'relay::reasoner')).toEqual([]);
  });

  test('maps the stored effort onto a stop, null being the none rung', () => {
    expect(stopOfEffort(claude, null)).toBe(0);
    expect(stopOfEffort(claude, 'xhigh')).toBe(4);
    expect(stopOfEffort(claude, 'HIGH')).toBe(3);
    // A ladder without a none rung falls back to medium.
    expect(stopOfEffort(['minimal', 'low', 'medium', 'high'], null)).toBe(2);
  });

  test('selecting the none rung clears the override, any other sets it', () => {
    expect(patchOfEffort('none')).toBeNull();
    expect(patchOfEffort('xhigh')).toBe('xhigh');
  });
});

describe('slider geometry', () => {
  test('snaps a track position to the nearest of evenly spaced stops', () => {
    expect(nearestStop(0, 6)).toBe(0);
    expect(nearestStop(0.49, 6)).toBe(2);
    expect(nearestStop(0.51, 6)).toBe(3);
    expect(nearestStop(1.4, 6)).toBe(5);
    expect(nearestStop(-1, 6)).toBe(0);
    expect(nearestStop(0.7, 1)).toBe(0);
    expect(stopFraction(5, 6)).toBe(1);
    expect(stopFraction(0, 1)).toBe(0);
  });

  test('arrow keys move one stop and stay on the ladder', () => {
    expect(stopAfterKey('ArrowRight', 2, 6)).toBe(3);
    expect(stopAfterKey('ArrowLeft', 2, 6)).toBe(1);
    expect(stopAfterKey('ArrowLeft', 0, 6)).toBe(0);
    expect(stopAfterKey('ArrowRight', 5, 6)).toBe(5);
    expect(stopAfterKey('Home', 3, 6)).toBe(0);
    expect(stopAfterKey('End', 3, 6)).toBe(5);
    expect(stopAfterKey('a', 3, 6)).toBeNull();
  });
});
