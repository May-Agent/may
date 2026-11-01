import { ApiError } from './client';

/** Masked result of a `!` bash-mode command run by the daemon. */
export interface BashOutcome {
  command: string;
  stdout: string;
  stderr: string;
  exitCode: number;
  hadSecret: boolean;
  error: string | null;
}

/** Run `command` on the user's behalf in the session's working directory. */
export async function runBash(sessionId: string, command: string): Promise<BashOutcome> {
  const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/bash`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ command }),
  });
  if (!res.ok) {
    const message = await res.text().catch(() => '');
    throw new ApiError('internal_error', message || `bash failed: ${res.status}`, res.status);
  }
  return (await res.json()) as BashOutcome;
}

/** The masked transcript the agent receives — the same shape the TUI sends. */
export function bashTranscript(outcome: BashOutcome): string {
  let content = `! ${outcome.command}\n`;
  if (outcome.error) {
    content += `\n<error>${outcome.error}</error>`;
  } else {
    if (outcome.stdout) content += `\n<stdout>\n${outcome.stdout}\n</stdout>`;
    if (outcome.stderr) content += `\n<stderr>\n${outcome.stderr}\n</stderr>`;
    if (outcome.exitCode !== 0) content += `\n<exit>${outcome.exitCode}</exit>`;
  }
  return content;
}
