import { useCallback, useEffect, useRef, useState } from 'react';
import * as DialogPrimitive from '@radix-ui/react-dialog';
import { Eye, Loader2, Minimize2, MonitorOff } from 'lucide-react';
import { toast } from 'sonner';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import { sendStreamInput, useBrowserStream } from '@/lib/stores/browserStream';
import {
  resizeBrowserViewport,
  startBrowser,
  startTakeover,
  stopTakeover,
} from '@/lib/api/browser';
import { LiveScreen } from './LiveScreen';

const MOUSE_BUTTON: Record<number, 'left' | 'right' | 'middle'> = {
  0: 'left',
  1: 'middle',
  2: 'right',
};

function modifiersOf(event: { ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean }): number {
  // CDP modifier bitmap: Alt=1, Ctrl=2, Meta/Cmd=4, Shift=8
  return (
    (event.altKey ? 1 : 0) |
    (event.ctrlKey ? 2 : 0) |
    (event.metaKey ? 4 : 0) |
    (event.shiftKey ? 8 : 0)
  );
}

/** Map a pointer position to the video's natural pixels (object-contain). */
function mapToVideo(video: HTMLVideoElement, clientX: number, clientY: number) {
  const rect = video.getBoundingClientRect();
  const naturalW = video.videoWidth;
  const naturalH = video.videoHeight;
  if (!naturalW || !naturalH) return null;
  const elAspect = rect.width / rect.height;
  const videoAspect = naturalW / naturalH;
  let w = rect.width;
  let h = rect.height;
  let ox = 0;
  let oy = 0;
  if (videoAspect > elAspect) {
    h = rect.width / videoAspect;
    oy = (rect.height - h) / 2;
  } else {
    w = rect.height * videoAspect;
    ox = (rect.width - w) / 2;
  }
  const x = clientX - rect.left - ox;
  const y = clientY - rect.top - oy;
  if (x < 0 || y < 0 || x > w || y > h) return null;
  return { x: (x / w) * naturalW, y: (y / h) * naturalH };
}

/**
 * The agent's browser, full screen: its viewport centered on a dark
 * window, a minimize control, and one Watching / Take control switch.
 * Taking control pauses the agent and sends your pointer and keys to the
 * browser; Watching hands it back.
 */
export function BrowserViewer() {
  const { t } = useTranslation();
  const open = useBrowserViewStore((s) => s.viewerOpen);
  const closeViewer = useBrowserViewStore((s) => s.closeViewer);
  const browser = useRuntimeStore((s) => s.browser);
  const lifecycle = useRuntimeStore((s) => s.browserLifecycle);
  const sessionId = useSessionsStore((s) => s.active?.id ?? null);

  const running = browser?.running ?? false;
  const shown = open && sessionId !== null;
  const { stream, state } = useBrowserStream(sessionId, shown && running);
  const takeover = sessionId !== null && browser?.takeoverSessionId === sessionId;

  const stageRef = useRef<HTMLDivElement | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [busy, setBusy] = useState(false);

  // Size the browser's viewport to the frame, debounced so a window drag
  // does not fire a resize per frame.
  useEffect(() => {
    if (!shown || !running) return;
    const stage = stageRef.current;
    if (!stage) return;
    const dpr = window.devicePixelRatio || 1;
    let last = '';
    let timer: ReturnType<typeof setTimeout> | null = null;
    const flush = () => {
      timer = null;
      const rect = stage.getBoundingClientRect();
      const width = Math.round(rect.width);
      const height = Math.round(rect.height);
      const key = `${width}x${height}@${dpr}`;
      if (width === 0 || height === 0 || key === last) return;
      last = key;
      void resizeBrowserViewport({ width, height, deviceScaleFactor: dpr }).catch(() => {
        // Best effort: the viewport keeps its size until the next change.
      });
    };
    const observer = new ResizeObserver(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(flush, 300);
    });
    observer.observe(stage);
    flush();
    return () => {
      observer.disconnect();
      if (timer) clearTimeout(timer);
    };
  }, [shown, running]);

  const onMouse = useCallback(
    (
      kind: 'mousePressed' | 'mouseReleased' | 'mouseMoved' | 'mouseWheel',
      event: React.MouseEvent<HTMLVideoElement> | React.WheelEvent<HTMLVideoElement>,
    ) => {
      if (!takeover || !videoRef.current) return;
      const at = mapToVideo(videoRef.current, event.clientX, event.clientY);
      if (!at) return;
      const modifiers = modifiersOf(event);
      if (kind === 'mouseWheel') {
        const wheel = event as React.WheelEvent<HTMLVideoElement>;
        sendStreamInput({ type: 'mouse', eventType: 'mouseWheel', ...at, deltaX: wheel.deltaX, deltaY: wheel.deltaY, modifiers });
        return;
      }
      const mouse = event as React.MouseEvent<HTMLVideoElement>;
      sendStreamInput({
        type: 'mouse',
        eventType: kind,
        ...at,
        button: MOUSE_BUTTON[mouse.button] ?? 'left',
        clickCount: kind === 'mousePressed' ? mouse.detail || 1 : undefined,
        modifiers,
      });
    },
    [takeover],
  );

  const onKey = useCallback(
    (kind: 'keyDown' | 'keyUp', event: React.KeyboardEvent<HTMLVideoElement>) => {
      if (!takeover) return;
      event.preventDefault();
      event.stopPropagation();
      const text =
        kind === 'keyDown' && event.key.length === 1 && !event.ctrlKey && !event.metaKey
          ? event.key
          : undefined;
      sendStreamInput({ type: 'keyboard', eventType: kind, key: event.key, code: event.code, text, modifiers: modifiersOf(event) });
    },
    [takeover],
  );

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await fn();
    } catch (err) {
      toast.error(t('errors.generic'), { description: err instanceof Error ? err.message : undefined });
    } finally {
      setBusy(false);
    }
  };

  const setMode = (control: boolean) => {
    if (!sessionId || control === takeover || busy) return;
    void run(() => (control ? startTakeover(sessionId) : stopTakeover(sessionId)));
  };

  const connecting = running && (!stream || state === 'connecting' || state === 'new');
  const lost = running && (state === 'failed' || state === 'closed' || state === 'disconnected');
  const caption = lifecycle.pageTitle ?? lifecycle.pageUrl;

  return (
    <DialogPrimitive.Root open={shown} onOpenChange={(next) => !next && closeViewer()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Content
          aria-describedby={undefined}
          className="fixed inset-0 z-50 flex flex-col bg-[#141414] text-white outline-none data-[state=open]:animate-in data-[state=open]:fade-in-0"
          onEscapeKeyDown={(e) => {
            // While you drive, Escape belongs to the page.
            if (takeover && document.activeElement === videoRef.current) e.preventDefault();
          }}
        >
          <DialogPrimitive.Title className="sr-only">{t('browser.title')}</DialogPrimitive.Title>
          <div className="flex h-12 shrink-0 items-center gap-2 pl-5 pr-3 touch:h-14">
            <p className="min-w-0 flex-1 truncate text-xs text-white/45" title={caption ?? undefined}>
              {caption}
            </p>
            <DialogPrimitive.Close
              aria-label={t('browser.minimize')}
              title={t('browser.minimize')}
              className="flex size-8 items-center justify-center rounded-md text-white/55 transition-colors hover:bg-white/10 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/40 touch:size-11"
            >
              <Minimize2 className="size-4" aria-hidden />
            </DialogPrimitive.Close>
          </div>

          <div className="flex min-h-0 flex-1 items-center justify-center px-4 pb-24 sm:px-16">
            <div
              ref={stageRef}
              className="relative h-full w-full max-w-[1440px] overflow-hidden rounded-xl bg-[#1e1e1e]"
            >
              {running && (
                <LiveScreen
                  ref={videoRef}
                  stream={stream}
                  tabIndex={takeover ? 0 : -1}
                  className={cn('h-full w-full object-contain outline-none', takeover && 'cursor-default')}
                  onMouseDown={(e) => onMouse('mousePressed', e)}
                  onMouseUp={(e) => onMouse('mouseReleased', e)}
                  onMouseMove={(e) => onMouse('mouseMoved', e)}
                  onWheel={(e) => onMouse('mouseWheel', e)}
                  onContextMenu={(e) => takeover && e.preventDefault()}
                  onKeyDown={(e) => onKey('keyDown', e)}
                  onKeyUp={(e) => onKey('keyUp', e)}
                />
              )}
              {(connecting || lost || !running) && (
                <div
                  role="status"
                  className="absolute inset-0 flex flex-col items-center justify-center gap-3 text-[13px] text-white/50"
                >
                  {!running ? (
                    <>
                      <MonitorOff className="size-5" strokeWidth={1.5} aria-hidden />
                      {t('browser.asleep')}
                      {browser?.enabled && (
                        <button
                          type="button"
                          disabled={busy}
                          onClick={() => void run(startBrowser)}
                          className="mt-1 h-7 rounded-md bg-white/10 px-3 text-xs font-medium text-white transition-colors hover:bg-white/15 disabled:opacity-50 touch:h-11"
                        >
                          {t('browser.wake')}
                        </button>
                      )}
                    </>
                  ) : lost ? (
                    t('browser.streamLost')
                  ) : (
                    <>
                      <Loader2 className="size-5 animate-spin" aria-hidden />
                      {t('browser.connecting')}
                    </>
                  )}
                </div>
              )}
            </div>
          </div>

          {running && (
            <div className="pointer-events-none absolute inset-x-0 bottom-6 flex justify-center">
              <div
                role="radiogroup"
                aria-label={t('browser.mode')}
                className="pointer-events-auto flex items-center gap-0.5 rounded-full bg-white p-1 text-[13px] font-medium shadow-[0_8px_24px_rgb(0_0_0/0.35)]"
              >
                <ModeSegment selected={!takeover} disabled={busy} onSelect={() => setMode(false)}>
                  <Eye className="size-4" aria-hidden />
                  {t('browser.watching')}
                </ModeSegment>
                <ModeSegment selected={takeover} disabled={busy} onSelect={() => setMode(true)}>
                  {t('browser.takeControl')}
                </ModeSegment>
              </div>
            </div>
          )}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}

function ModeSegment({
  selected,
  disabled,
  onSelect,
  children,
}: {
  selected: boolean;
  disabled: boolean;
  onSelect: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      disabled={disabled && !selected}
      onClick={onSelect}
      className={cn(
        'flex h-8 items-center gap-1.5 rounded-full px-3.5 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#141414]/30 touch:h-11 touch:px-4',
        selected ? 'bg-[#141414] text-white' : 'text-neutral-500 hover:text-neutral-900',
      )}
    >
      {children}
    </button>
  );
}
