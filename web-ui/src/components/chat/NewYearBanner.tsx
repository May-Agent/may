import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import { PartyPopper, X } from 'lucide-react';
import { useTranslation } from '@/lib/i18n';
import { useMediaQuery } from '@/lib/useMediaQuery';
import {
  FIREWORKS_IDLE,
  createShow,
  fireworksReducer,
  isNewYearDay,
  showDone,
  stepShow,
} from '@/lib/newYear';

const REDUCED_MOTION_QUERY = '(prefers-reduced-motion: reduce)';

interface NewYearBannerProps {
  /** IANA name of the user's timezone (`[time] timezone`). */
  timeZone: string;
  /** The current time; the page passes the clock reading, tests pass a fixed one. */
  now: Date;
}

/**
 * A greeting above the composer on December 31 and January 1 in the user's
 * timezone. Clicking it plays fireworks on a canvas over the page; the show
 * ends by itself or on any click or key. Under `prefers-reduced-motion` the
 * greeting is static.
 */
export function NewYearBanner({ timeZone, now }: NewYearBannerProps) {
  const { t } = useTranslation();
  const reduceMotion = useMediaQuery(REDUCED_MOTION_QUERY);
  const [fireworks, dispatch] = useReducer(fireworksReducer, FIREWORKS_IDLE);
  const [dismissed, setDismissed] = useState(false);
  const end = useCallback(() => dispatch({ type: 'end' }), []);

  if (dismissed || !isNewYearDay(now, timeZone)) return null;

  return (
    <div
      data-testid="new-year-banner"
      className="flex items-center gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm"
    >
      {reduceMotion ? (
        <span className="flex flex-1 items-center gap-2">
          <PartyPopper className="size-4 text-amber-500" aria-hidden />
          {t('newYear.greeting')}
        </span>
      ) : (
        <button
          type="button"
          className="flex flex-1 items-center gap-2 text-left"
          onClick={() => dispatch({ type: 'start' })}
        >
          <PartyPopper className="size-4 text-amber-500" aria-hidden />
          <span>{t('newYear.greeting')}</span>
          <span className="text-xs text-muted-foreground">{t('newYear.play')}</span>
        </button>
      )}
      <button
        type="button"
        aria-label={t('newYear.dismiss')}
        className="text-muted-foreground hover:text-foreground"
        onClick={() => setDismissed(true)}
      >
        <X className="size-4" aria-hidden />
      </button>
      {fireworks.playing && <FireworksCanvas onEnd={end} />}
    </div>
  );
}

function FireworksCanvas({ onEnd }: { onEnd: () => void }) {
  const { t } = useTranslation();
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext('2d');
    if (!canvas || !ctx) {
      onEnd();
      return;
    }
    const dpr = window.devicePixelRatio || 1;
    const width = window.innerWidth;
    const height = window.innerHeight;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    ctx.scale(dpr, dpr);

    let show = createShow(width, height, Math.random);
    let last = performance.now();
    let frame = 0;
    const tick = (time: number) => {
      show = stepShow(show, Math.min(time - last, 50), Math.random);
      last = time;
      ctx.clearRect(0, 0, width, height);
      const fade = Math.max(0, Math.min(1, 1 - (show.elapsed - 2400) / 1600));
      for (const spark of show.sparks) {
        ctx.fillStyle = `hsla(${spark.hue}, 100%, 65%, ${fade})`;
        ctx.fillRect(spark.x, spark.y, 3, 3);
      }
      if (showDone(show)) {
        onEnd();
        return;
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    window.addEventListener('keydown', onEnd);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener('keydown', onEnd);
    };
  }, [onEnd]);

  return (
    <canvas
      ref={canvasRef}
      aria-label={t('newYear.stop')}
      className="fixed inset-0 z-50 size-full"
      onClick={onEnd}
    />
  );
}
