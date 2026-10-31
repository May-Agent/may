/** A gap this long between two messages starts a new group with its own time. */
export const GROUP_GAP_MS = 15 * 60 * 1000;

function parse(timestamp: string | undefined): number | null {
  if (!timestamp) return null;
  const ms = Date.parse(timestamp);
  return Number.isNaN(ms) ? null : ms;
}

/** Whether a centered time label goes before the message at `current`. */
export function startsGroup(previous: string | undefined, current: string | undefined): boolean {
  const cur = parse(current);
  if (cur === null) return false;
  const prev = parse(previous);
  return prev === null || cur - prev >= GROUP_GAP_MS;
}

function sameDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** "9:12 AM" today, "Tue 9:12 AM" this week, "Oct 3, 9:12 AM" before. */
export function formatGroupTime(timestamp: string, now: Date, locale?: string): string {
  const ms = parse(timestamp);
  if (ms === null) return '';
  const date = new Date(ms);
  const time: Intl.DateTimeFormatOptions = { hour: 'numeric', minute: '2-digit' };
  if (sameDay(date, now)) return new Intl.DateTimeFormat(locale, time).format(date);
  const days = (now.getTime() - ms) / 86_400_000;
  if (days >= 0 && days < 6) {
    return new Intl.DateTimeFormat(locale, { weekday: 'short', ...time }).format(date);
  }
  return new Intl.DateTimeFormat(locale, { month: 'short', day: 'numeric', ...time }).format(date);
}
