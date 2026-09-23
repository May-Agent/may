import { useCallback, useEffect, useRef, useState } from 'react';
import { ChevronsRight, Play, Power, Square } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import {
  type BrowserInputMessage,
  type BrowserStreamHandle,
  connectBrowserStream,
  resizeBrowserViewport,
  sendBrowserInput,
  startBrowser,
  startTakeover,
  stopTakeover,
} from '@/lib/api/browser';
import { BrowserLifecycleStatus } from './BrowserLifecycleStatus';

const MOUSE_BUTTON: Record<number, 'left' | 'right' | 'middle'> = {
  0: 'left',
  1: 'middle',
  2: 'right',
};

function modifiersFromMouse(event: { ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean }): number {
  // CDP modifier bitmap: Alt=1, Ctrl=2, Meta/Cmd=4, Shift=8
  return (
    (event.altKey ? 1 : 0) |
    (event.ctrlKey ? 2 : 0) |
    (event.metaKey ? 4 : 0) |
    (event.shiftKey ? 8 : 0)
  );
}

interface VideoCoords {
  x: number;
  y: number;
}

function mapEventToVideo(
  videoEl: HTMLVideoElement,
  clientX: number,
  clientY: number,
): VideoCoords | null {
  const rect = videoEl.getBoundingClientRect();
  const naturalW = videoEl.videoWidth;
  const naturalH = videoEl.videoHeight;
  if (!naturalW || !naturalH) return null;
  // Letterbox: object-contain. Compute the displayed video rect inside
  // the element so input maps to the natural pixel space.
  const elAspect = rect.width / rect.height;
  const videoAspect = naturalW / naturalH;
  let displayedW = rect.width;
  let displayedH = rect.height;
  let offsetX = 0;
  let offsetY = 0;
  if (videoAspect > elAspect) {
    displayedH = rect.width / videoAspect;
    offsetY = (rect.height - displayedH) / 2;
  } else {
    displayedW = rect.height * videoAspect;
    offsetX = (rect.width - displayedW) / 2;
  }
  const xInDisplayed = clientX - rect.left - offsetX;
  const yInDisplayed = clientY - rect.top - offsetY;
  if (
    xInDisplayed < 0 ||
    yInDisplayed < 0 ||
    xInDisplayed > displayedW ||
    yInDisplayed > displayedH
  ) {
    return null;
  }
  return {
    x: (xInDisplayed / displayedW) * naturalW,
    y: (yInDisplayed / displayedH) * naturalH,
  };
}

export function BrowserLiveView() {
  const { t } = useTranslation();
  const open = useBrowserViewStore((s) => s.open);
  const setOpen = useBrowserViewStore((s) => s.setOpen);
  const browser = useRuntimeStore((s) => s.browser);
  const lifecycle = useRuntimeStore((s) => s.browserLifecycle);
  const activeSession = useSessionsStore((s) => s.active);

  const videoRef = useRef<HTMLVideoElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const handleRef = useRef<BrowserStreamHandle | null>(null);
  const [connectionState, setConnectionState] = useState<RTCPeerConnectionState>('new');
  const [busy, setBusy] = useState(false);

  const sessionId = activeSession?.id ?? null;
  const isRunning = browser?.running ?? false;
  const takeoverActive = browser?.takeoverSessionId === sessionId && sessionId !== null;

  // Track the panel's rendered pixel size and ask the actor to resize
  // the viewport to match. Debounced so a single drag-resize doesn't fire
  // dozens of CDP round-trips.
  useEffect(() => {
    if (!open || !isRunning) return;
    const stage = stageRef.current;
    if (!stage) return;
    const dpr = typeof window !== 'undefined' ? window.devicePixelRatio || 1 : 1;
    let lastSent: { width: number; height: number; dpr: number } | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const flush = () => {
      timer = null;
      const rect = stage.getBoundingClientRect();
      const width = Math.round(rect.width);
      const height = Math.round(rect.height);
      if (width === 0 || height === 0) return;
      if (
        lastSent &&
        lastSent.width === width &&
        lastSent.height === height &&
        lastSent.dpr === dpr
      ) {
        return;
      }
      lastSent = { width, height, dpr };
      void resizeBrowserViewport({
        width,
        height,
        deviceScaleFactor: dpr,
      }).catch(() => {
        // Resize is best-effort — a transient failure just means the
        // viewport stays at the previous resolution until the next
        // observer tick.
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
  }, [open, isRunning]);

  // Establish / tear down the WebRTC connection in step with panel
  // visibility, browser running state, and the active session.
  useEffect(() => {
    if (!open || !isRunning || !sessionId) return;
    let cancelled = false;
    const video = videoRef.current;
    (async () => {
      try {
        const handle = await connectBrowserStream({
          sessionId,
          onTrack: (stream) => {
            if (video) video.srcObject = stream;
          },
          onConnectionStateChange: (state) => {
            setConnectionState(state);
          },
        });
        if (cancelled) {
          handle.close();
          return;
        }
        handleRef.current = handle;
      } catch (err) {
        if (!cancelled) {
          toast.error(t('browserLive.streamLost'), {
            description: err instanceof Error ? err.message : undefined,
          });
        }
      }
    })();
    return () => {
      cancelled = true;
      handleRef.current?.close();
      handleRef.current = null;
      if (video) video.srcObject = null;
      setConnectionState('new');
    };
  }, [open, isRunning, sessionId, t]);

  const sendInput = useCallback((msg: BrowserInputMessage) => {
    const dc = handleRef.current?.dc;
    if (!dc) return;
    sendBrowserInput(dc, msg);
  }, []);

  const onMouseEvent = useCallback(
    (
      kind: 'mousePressed' | 'mouseReleased' | 'mouseMoved' | 'mouseWheel',
      event: React.MouseEvent<HTMLVideoElement> | React.WheelEvent<HTMLVideoElement>,
    ) => {
      if (!takeoverActive) return;
      const video = videoRef.current;
      if (!video) return;
      const coords = mapEventToVideo(video, event.clientX, event.clientY);
      if (!coords) return;
      const button = MOUSE_BUTTON[(event as React.MouseEvent).button ?? 0] ?? 'left';
      const modifiers = modifiersFromMouse(event);
      if (kind === 'mouseWheel') {
        const wheel = event as React.WheelEvent<HTMLVideoElement>;
        sendInput({
          type: 'mouse',
          eventType: 'mouseWheel',
          x: coords.x,
          y: coords.y,
          deltaX: wheel.deltaX,
          deltaY: wheel.deltaY,
          modifiers,
        });
        return;
      }
      const mouse = event as React.MouseEvent<HTMLVideoElement>;
      sendInput({
        type: 'mouse',
        eventType: kind,
        x: coords.x,
        y: coords.y,
        button,
        clickCount: kind === 'mousePressed' ? mouse.detail || 1 : undefined,
        modifiers,
      });
    },
    [takeoverActive, sendInput],
  );

  const onKey = useCallback(
    (kind: 'keyDown' | 'keyUp', event: React.KeyboardEvent<HTMLVideoElement>) => {
      if (!takeoverActive) return;
      event.preventDefault();
      const text =
        kind === 'keyDown' && event.key.length === 1 && !event.ctrlKey && !event.metaKey
          ? event.key
          : undefined;
      sendInput({
        type: 'keyboard',
        eventType: kind,
        key: event.key,
        code: event.code,
        text,
        modifiers: modifiersFromMouse(event),
      });
    },
    [takeoverActive, sendInput],
  );

  const handleStart = useCallback(async () => {
    setBusy(true);
    try {
      await startBrowser();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  }, [t]);

  const handleTakeoverToggle = useCallback(async () => {
    if (!sessionId) return;
    setBusy(true);
    try {
      if (takeoverActive) {
        await stopTakeover(sessionId);
      } else {
        await startTakeover(sessionId);
      }
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  }, [sessionId, takeoverActive, t]);

  if (!open) return null;

  return (
    <aside
      className="flex h-full w-[480px] shrink-0 flex-col border-l border-border bg-background"
      data-testid="browser-live-view"
    >
      <header className="flex items-center justify-between gap-2 border-b border-border px-3 py-2">
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <div className="flex items-center gap-2 text-sm font-medium">
            <span className="truncate">{t('browserLive.panelTitle')}</span>
          </div>
          <BrowserLifecycleStatus snapshot={lifecycle} />
        </div>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          aria-label={t('browserLive.close')}
          onClick={() => setOpen(false)}
        >
          <ChevronsRight className="size-4" />
        </Button>
      </header>

      <div ref={stageRef} className="relative flex-1 overflow-hidden bg-black/95">
        {!isRunning && (
          <div className="flex h-full w-full flex-col items-center justify-center gap-3 text-sm text-muted-foreground">
            <span>{t('browserLive.notRunning')}</span>
            <Button type="button" disabled={busy} onClick={handleStart}>
              <Play className="size-4" />
              {t('browserLive.start')}
            </Button>
          </div>
        )}
        {isRunning && (
          <video
            ref={videoRef}
            autoPlay
            playsInline
            muted
            tabIndex={takeoverActive ? 0 : -1}
            className={cn(
              'h-full w-full object-contain outline-none',
              takeoverActive && 'cursor-crosshair',
            )}
            onMouseDown={(e) => onMouseEvent('mousePressed', e)}
            onMouseUp={(e) => onMouseEvent('mouseReleased', e)}
            onMouseMove={(e) => onMouseEvent('mouseMoved', e)}
            onWheel={(e) => onMouseEvent('mouseWheel', e)}
            onContextMenu={(e) => takeoverActive && e.preventDefault()}
            onKeyDown={(e) => onKey('keyDown', e)}
            onKeyUp={(e) => onKey('keyUp', e)}
          />
        )}
        {isRunning && connectionState !== 'connected' && (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-xs text-white/80">
            {connectionState === 'failed' || connectionState === 'closed'
              ? t('browserLive.streamLost')
              : t('browserLive.connecting')}
          </div>
        )}
      </div>

      <footer className="flex items-center justify-end gap-2 border-t border-border px-3 py-2">
        {isRunning && sessionId && (
          <Button
            type="button"
            size="sm"
            variant={takeoverActive ? 'default' : 'outline'}
            disabled={busy}
            onClick={handleTakeoverToggle}
          >
            {takeoverActive ? <Square className="size-3.5" /> : <Power className="size-3.5" />}
            {takeoverActive ? t('browserLive.release') : t('browserLive.takeOver')}
          </Button>
        )}
      </footer>
    </aside>
  );
}
