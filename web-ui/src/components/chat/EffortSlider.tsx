import { useRef, type KeyboardEvent, type PointerEvent } from 'react';
import { nearestStop, stopAfterKey, stopFraction } from '@/lib/effort';

interface EffortSliderProps {
  levels: readonly string[];
  /** Index of the selected stop. */
  value: number;
  /** Name shown for a level. */
  labelOf: (level: string) => string;
  /** The thumb moved to a stop; the choice is not final yet. */
  onChange: (stop: number) => void;
  /** The choice is final: pointer released or Enter pressed. */
  onCommit: (stop: number) => void;
  ariaLabel: string;
}

/** Thumb diameter in px; the track is inset by its radius so the thumb stays inside. */
const THUMB = 24;

const position = (fraction: number) =>
  `calc(${THUMB / 2}px + (100% - ${THUMB}px) * ${fraction})`;

/** A horizontal slider over an ordered list of levels, one evenly spaced stop per level. */
export function EffortSlider({
  levels,
  value,
  labelOf,
  onChange,
  onCommit,
  ariaLabel,
}: EffortSliderProps) {
  const trackRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);
  const count = levels.length;
  const fraction = stopFraction(value, count);

  const stopAt = (event: PointerEvent) => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= THUMB) return value;
    const inner = (event.clientX - rect.left - THUMB / 2) / (rect.width - THUMB);
    return nearestStop(inner, count);
  };

  const move = (stop: number) => {
    if (stop !== value) onChange(stop);
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      onCommit(value);
      return;
    }
    const next = stopAfterKey(event.key, value, count);
    if (next === null) return;
    event.preventDefault();
    move(next);
  };

  return (
    <div
      ref={trackRef}
      role="slider"
      tabIndex={0}
      aria-label={ariaLabel}
      aria-orientation="horizontal"
      aria-valuemin={0}
      aria-valuemax={count - 1}
      aria-valuenow={value}
      aria-valuetext={labelOf(levels[value] ?? '')}
      className="relative h-8 w-full cursor-pointer touch-none select-none outline-none focus-visible:ring-2 focus-visible:ring-blue-500/40 rounded-full"
      onKeyDown={onKeyDown}
      onPointerDown={(event) => {
        dragging.current = true;
        event.currentTarget.setPointerCapture(event.pointerId);
        move(stopAt(event));
      }}
      onPointerMove={(event) => {
        if (dragging.current) move(stopAt(event));
      }}
      onPointerUp={(event) => {
        if (!dragging.current) return;
        dragging.current = false;
        event.currentTarget.releasePointerCapture(event.pointerId);
        onCommit(stopAt(event));
      }}
      onPointerCancel={() => {
        dragging.current = false;
      }}
    >
      <div className="absolute inset-x-0 top-1/2 h-2 -translate-y-1/2 rounded-full bg-muted" />
      <div
        className="absolute left-0 top-1/2 h-2 -translate-y-1/2 rounded-full bg-blue-500"
        style={{ width: position(fraction) }}
      />
      {levels.map((level, stop) => (
        <span
          key={level}
          data-stop={level}
          aria-hidden="true"
          className={
            stop <= value
              ? 'absolute top-1/2 size-1 -translate-x-1/2 -translate-y-1/2 rounded-full bg-white/60'
              : 'absolute top-1/2 size-1 -translate-x-1/2 -translate-y-1/2 rounded-full bg-muted-foreground/40'
          }
          style={{ left: position(stopFraction(stop, count)) }}
        />
      ))}
      <span
        data-thumb=""
        aria-hidden="true"
        className="absolute top-1/2 -translate-x-1/2 -translate-y-1/2 rounded-full border border-black/5 bg-white shadow-md"
        style={{ left: position(fraction), width: THUMB, height: THUMB }}
      />
    </div>
  );
}
