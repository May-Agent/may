import { apiDelete, apiPost } from './client';
import type { SleepStatus } from '../types';

/** `/sleep`: marks the user asleep for every session; the daemon starts the good-night turn. */
export function startSleep(sessionId: string): Promise<SleepStatus> {
  return apiPost<SleepStatus>('/sleep', { sessionId });
}

/** `/sleep off`: ends the sleep state. */
export function endSleep(): Promise<SleepStatus> {
  return apiDelete<SleepStatus>('/sleep');
}

/** What a client remembers of the reminder counter between `sleep` events. */
export interface ReminderBaseline {
  /** The counter at the last report; `null` before the first one. */
  seen: number | null;
}

/**
 * Applies one `sleep` report to the baseline. The first report only sets the
 * baseline, so a reminder raised before this client attached is not shown;
 * afterwards the reminder shows whenever the counter grows.
 */
export function applyReminderReport(
  baseline: ReminderBaseline,
  reminders: number,
): { baseline: ReminderBaseline; remind: boolean } {
  return {
    baseline: { seen: reminders },
    remind: baseline.seen !== null && reminders > baseline.seen,
  };
}

/** Whether the composer text is a sleep command, and which. */
export function parseSleepCommand(content: string): 'sleep' | 'off' | null {
  const text = content.trim();
  if (text === '/sleep') return 'sleep';
  if (text === '/sleep off') return 'off';
  return null;
}
