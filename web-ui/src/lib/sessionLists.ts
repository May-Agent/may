import type { ProfileSessionSummary, ProfileSessions } from './types';

/** Every session of the recent list: pinned (folders flattened) then regular. */
export function recentSessions(sessions: ProfileSessions): ProfileSessionSummary[] {
  const pinned = sessions.pinned.flatMap((item) =>
    item.type === 'session' ? [item] : item.sessions,
  );
  return [...pinned, ...sessions.regular];
}

/** The archived sessions, the ones the archive page lists. */
export function archivedSessions(sessions: ProfileSessions): ProfileSessionSummary[] {
  return sessions.archived;
}

/** Whether something runs in the session; the daemon decides. */
export function isWorking(session: Pick<ProfileSessionSummary, 'working'>): boolean {
  return session.working === true;
}
