/**
 * Whether a turn's error `code` reports a turn that stopped for a reason the
 * user is told, with nothing failed: the user declined the session's
 * repository question, or the daemon ended a turn whose agent kept looping.
 * Such a turn shows its reason as a notice, not as an error.
 */
export function isTurnStop(code: string): boolean {
  return code === 'repo_declined' || code === 'loop_stopped';
}
