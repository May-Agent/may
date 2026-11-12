/** Whether `now` falls on December 31 or January 1 in the IANA zone `timeZone`. */
export function isNewYearDay(now: Date, timeZone: string): boolean {
  const parts = new Intl.DateTimeFormat('en-US', {
    timeZone,
    month: 'numeric',
    day: 'numeric',
  }).formatToParts(now);
  const month = Number(parts.find((p) => p.type === 'month')?.value);
  const day = Number(parts.find((p) => p.type === 'day')?.value);
  return (month === 12 && day === 31) || (month === 1 && day === 1);
}

/** Whether the fireworks are playing; the page underneath never changes. */
export interface FireworksState {
  playing: boolean;
}

export type FireworksAction = { type: 'start' } | { type: 'end' };

export const FIREWORKS_IDLE: FireworksState = { playing: false };

export function fireworksReducer(state: FireworksState, action: FireworksAction): FireworksState {
  switch (action.type) {
    case 'start':
      return state.playing ? state : { playing: true };
    case 'end':
      return state.playing ? FIREWORKS_IDLE : state;
  }
}

export interface Spark {
  x: number;
  y: number;
  vx: number;
  vy: number;
  hue: number;
}

export interface Show {
  /** Milliseconds since the show began. */
  elapsed: number;
  /** Bursts not yet fired, ascending by `at`. */
  pending: Array<{ at: number; x: number; y: number; hue: number }>;
  sparks: Spark[];
}

/** How long a show runs. */
export const SHOW_MS = 4000;
const SPARKS_PER_BURST = 48;
const GRAVITY = 60;
const BURST_COUNT = 7;

/** A show over a `width` x `height` canvas; `random` yields values in [0, 1). */
export function createShow(width: number, height: number, random: () => number): Show {
  const pending = Array.from({ length: BURST_COUNT }, (_, i) => ({
    at: (i * (SHOW_MS - 1600)) / (BURST_COUNT - 1) + random() * 150,
    x: width * (0.15 + random() * 0.7),
    y: height * (0.15 + random() * 0.4),
    hue: Math.floor(random() * 360),
  }));
  return { elapsed: 0, pending, sparks: [] };
}

/** Advance the show by `dtMs`: fire due bursts, move sparks. */
export function stepShow(show: Show, dtMs: number, random: () => number): Show {
  const elapsed = show.elapsed + dtMs;
  const due = show.pending.filter((b) => b.at <= elapsed);
  const pending = show.pending.filter((b) => b.at > elapsed);
  const dt = dtMs / 1000;
  // The last frame leaves no spark behind.
  const moved =
    elapsed >= SHOW_MS
      ? []
      : show.sparks.map((s) => ({
          ...s,
          x: s.x + s.vx * dt,
          y: s.y + s.vy * dt,
          vy: s.vy + GRAVITY * dt,
        }));
  const fresh = due.flatMap((b) =>
    Array.from({ length: SPARKS_PER_BURST }, (_, i) => {
      const angle = (2 * Math.PI * i) / SPARKS_PER_BURST;
      const speed = 60 + random() * 120;
      return { x: b.x, y: b.y, vx: Math.cos(angle) * speed, vy: Math.sin(angle) * speed, hue: b.hue };
    }),
  );
  return { elapsed, pending, sparks: [...moved, ...fresh] };
}

export function showDone(show: Show): boolean {
  return show.elapsed >= SHOW_MS;
}
