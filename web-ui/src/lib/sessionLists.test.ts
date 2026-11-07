import { describe, expect, test } from 'bun:test';
import { archivedSessions, isWorking, recentSessions } from './sessionLists';
import type { ProfileSessionSummary, ProfileSessions } from './types';

function summary(id: string, over: Partial<ProfileSessionSummary> = {}): ProfileSessionSummary {
  return { id, title: id, runtime: {}, ...over };
}

function sessions(): ProfileSessions {
  return {
    profile: {} as ProfileSessions['profile'],
    pinned: [
      { type: 'session', ...summary('pinned') },
      { type: 'folder', id: 'f', name: 'F', sessions: [summary('in-folder')] },
    ],
    regular: [summary('recent', { working: true })],
    archived: [summary('old')],
  };
}

describe('session lists', () => {
  test('the recent list holds pinned and regular sessions, not archived ones', () => {
    const ids = recentSessions(sessions()).map((s) => s.id);
    expect(ids).toEqual(['pinned', 'in-folder', 'recent']);
  });

  test('the archive page lists the archived sessions only', () => {
    expect(archivedSessions(sessions()).map((s) => s.id)).toEqual(['old']);
  });

  test('a session works when the daemon says so', () => {
    const [, , recent] = recentSessions(sessions());
    expect(isWorking(recent)).toBe(true);
    expect(isWorking(summary('idle'))).toBe(false);
  });
});
