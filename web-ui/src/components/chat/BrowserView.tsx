import { useCallback, useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { cn } from '@/lib/utils';
import {
  startBrowser,
  browserOffer,
  browserTakeoverStart,
  browserTakeoverStop,
} from '@/lib/api';
import { useStore } from '@/lib/store';
import { Globe, Loader2, ArrowRight, X, MousePointerClick } from 'lucide-react';

/** Minimum interval between mouse-move dispatches (ms). 60 Hz matches typical displays. */
const MOUSE_MOVE_THROTTLE_MS = 16;

interface BrowserViewProps {
  open: boolean;
  onClose: () => void;
  sessionId: string | null;
}

/** JSON input message sent over the WebRTC DataChannel. */
interface InputMessage {
  type: 'mouse' | 'keyboard' | 'navigate';
  [key: string]: unknown;
}

export function BrowserView({ open, onClose, sessionId }: BrowserViewProps) {
  const [loading, setLoading] = useState(false);
  const [urlInput, setUrlInput] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  // takeoverActive mirrors takeoverSessionRef for re-renders. Flipped to true
  // when the user clicks the "Click to take control" overlay, flipped back
  // to false on release (tab hidden, panel close, Esc, component unmount).
  const [takeoverActive, setTakeoverActive] = useState(false);
  const browserStatus = useStore((state) => state.browserStatus);
  const browserEnabled = browserStatus?.enabled ?? false;
  const running = browserStatus?.running ?? false;
  const videoRef = useRef<HTMLVideoElement>(null);
  const peerRef = useRef<RTCPeerConnection | null>(null);
  const dataChannelRef = useRef<RTCDataChannel | null>(null);
  const lastMouseMoveRef = useRef<number>(0);
  const takeoverSessionRef = useRef<string | null>(null);
  const reconnectTimerRef = useRef<number | null>(null);
  const reconnectAttemptRef = useRef(0);
  const hasOpenedRef = useRef(false);

  useEffect(() => {
    if (!open) {
      if (!hasOpenedRef.current) {
        return;
      }
      if (peerRef.current) {
        peerRef.current.close();
        peerRef.current = null;
        dataChannelRef.current = null;
      }
      if (reconnectTimerRef.current != null) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      setConnected(false);
      return;
    }

    hasOpenedRef.current = true;
    setError(null);
  }, [open]);

  const handleStartBrowser = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      await startBrowser();
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
    } finally {
      setLoading(false);
    }
  }, []);

  const releaseTakeover = useCallback(async () => {
    const activeSessionId = takeoverSessionRef.current;
    setTakeoverActive(false);
    if (!activeSessionId) {
      return;
    }
    takeoverSessionRef.current = null;
    await browserTakeoverStop(activeSessionId);
  }, []);

  useEffect(() => {
    if (!open) {
      void releaseTakeover();
    }
  }, [open, releaseTakeover]);

  useEffect(
    () => () => {
      void releaseTakeover();
    },
    [releaseTakeover],
  );

  // Auto-release takeover when the tab becomes hidden so a backgrounded tab
  // doesn't silently hold the browser away from the agent.
  useEffect(() => {
    if (!takeoverActive) return;
    const onVisibility = () => {
      if (document.hidden) {
        void releaseTakeover();
      }
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => document.removeEventListener('visibilitychange', onVisibility);
  }, [takeoverActive, releaseTakeover]);

  // Esc releases takeover too — user shouldn't have to hunt for an exit.
  useEffect(() => {
    if (!takeoverActive) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        void releaseTakeover();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [takeoverActive, releaseTakeover]);

  const acquireTakeover = useCallback(async () => {
    if (!sessionId) {
      setError('Browser takeover requires an active session');
      return;
    }
    if (takeoverSessionRef.current === sessionId) {
      setTakeoverActive(true);
      return;
    }

    try {
      const previousSessionId = takeoverSessionRef.current;
      if (previousSessionId) {
        await browserTakeoverStop(previousSessionId);
      }
      await browserTakeoverStart(sessionId);
      takeoverSessionRef.current = sessionId;
      setTakeoverActive(true);
      setError(null);
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    }
  }, [sessionId]);

  // Send a message over the DataChannel. Gated on explicit takeover — without
  // it, hovering or typing over the video is a no-op.
  const sendInput = useCallback((msg: InputMessage) => {
    if (!takeoverActive) return;
    const dc = dataChannelRef.current;
    if (!dc || dc.readyState !== 'open') {
      return;
    }
    try {
      dc.send(JSON.stringify(msg));
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    }
  }, [takeoverActive]);

  // Establish WebRTC connection when running and sheet is open
  const connectWebRTC = useCallback(async function connectWebRTCImpl() {
    // Clean up any existing connection
    if (peerRef.current) {
      peerRef.current.close();
      peerRef.current = null;
      dataChannelRef.current = null;
    }
    if (reconnectTimerRef.current != null) {
      window.clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }

    const pc = new RTCPeerConnection({
      iceServers: [{ urls: 'stun:stun.l.google.com:19302' }],
    });
    peerRef.current = pc;

    // Create DataChannel for input forwarding (must be created BEFORE offer)
    const dc = pc.createDataChannel('input', { ordered: true });
    dataChannelRef.current = dc;
    dc.onopen = () => setConnected(true);
    dc.onclose = () => setConnected(false);

    // Bind incoming video tracks explicitly. Some browsers may deliver
    // tracks without a populated streams[] array for server-originated tracks.
    pc.ontrack = (ev) => {
      if (ev.track.kind !== 'video') {
        return;
      }
      const video = videoRef.current;
      if (!video) {
        return;
      }
      const stream = video.srcObject instanceof MediaStream
        ? video.srcObject
        : (ev.streams[0] ?? new MediaStream());
      if (!stream.getVideoTracks().some((track) => track.id === ev.track.id)) {
        stream.addTrack(ev.track);
      }
      video.srcObject = stream;
      void video.play().catch(() => {});
    };

    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'connected') {
        reconnectAttemptRef.current = 0;
      }
      if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
        setConnected(false);
        setError(`WebRTC connection ${pc.connectionState}`);
        if (reconnectAttemptRef.current >= 5 || reconnectTimerRef.current != null) {
          return;
        }
        const delayMs = Math.min(1000 * 2 ** reconnectAttemptRef.current, 10000);
        reconnectTimerRef.current = window.setTimeout(() => {
          reconnectTimerRef.current = null;
          reconnectAttemptRef.current += 1;
          void connectWebRTCImpl().catch((error: unknown) => {
            setError(error instanceof Error ? error.message : String(error));
          });
        }, delayMs);
      }
    };

    // Add recvonly media transceivers for synchronized AV streaming.
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });

    // Create offer
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    // Send offer to server and get answer (WHEP-style)
    const answerSdp = await browserOffer(offer.sdp!);
    await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
  }, []);

  // Connect/disconnect when running state changes
  useEffect(() => {
    if (!open || !running) {
      if (peerRef.current) {
        peerRef.current.close();
        peerRef.current = null;
        dataChannelRef.current = null;
        setConnected(false);
      }
      return;
    }

    connectWebRTC().catch((e) => {
      setError(e instanceof Error ? e.message : String(e));
    });

    return () => {
      if (peerRef.current) {
        peerRef.current.close();
        peerRef.current = null;
        dataChannelRef.current = null;
        setConnected(false);
      }
      if (reconnectTimerRef.current != null) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
    };
  }, [open, running, connectWebRTC]);

  const handleNavigate = async () => {
    if (!urlInput.trim()) return;
    let url = urlInput.trim();
    if (!/^https?:\/\//i.test(url)) {
      url = `https://${url}`;
    }
    // Typing a URL is explicit intent to direct the browser — acquire takeover
    // if not already active, then dispatch.
    if (!takeoverActive) {
      await acquireTakeover();
    }
    sendInput({ type: 'navigate', url });
  };

  // ── Input forwarding via DataChannel ──────────────────────

  const getVideoCoords = useCallback((e: React.MouseEvent<HTMLVideoElement>) => {
    const video = videoRef.current;
    if (!video) return { x: 0, y: 0 };
    const rect = video.getBoundingClientRect();
    // Map from display coordinates to the video's natural resolution
    const scaleX = video.videoWidth / rect.width;
    const scaleY = video.videoHeight / rect.height;
    return {
      x: (e.clientX - rect.left) * scaleX,
      y: (e.clientY - rect.top) * scaleY,
    };
  }, []);

  const getModifiers = useCallback((e: React.MouseEvent | React.KeyboardEvent) => {
    let m = 0;
    if (e.altKey) m |= 1;
    if (e.ctrlKey) m |= 2;
    if (e.metaKey) m |= 4;
    if (e.shiftKey) m |= 8;
    return m;
  }, []);

  const handleMouseDown = useCallback((e: React.MouseEvent<HTMLVideoElement>) => {
    const { x, y } = getVideoCoords(e);
    void sendInput({
      type: 'mouse',
      eventType: 'mousePressed',
      x,
      y,
      button: 'left',
      clickCount: e.detail,
      modifiers: getModifiers(e),
    });
  }, [getVideoCoords, getModifiers, sendInput]);

  const handleMouseUp = useCallback((e: React.MouseEvent<HTMLVideoElement>) => {
    const { x, y } = getVideoCoords(e);
    void sendInput({
      type: 'mouse',
      eventType: 'mouseReleased',
      x,
      y,
      button: 'left',
      clickCount: 1,
      modifiers: getModifiers(e),
    });
  }, [getVideoCoords, getModifiers, sendInput]);

  const handleMouseMove = useCallback((e: React.MouseEvent<HTMLVideoElement>) => {
    const now = performance.now();
    if (now - lastMouseMoveRef.current < MOUSE_MOVE_THROTTLE_MS) return;
    lastMouseMoveRef.current = now;

    const { x, y } = getVideoCoords(e);
    void sendInput({
      type: 'mouse',
      eventType: 'mouseMoved',
      x,
      y,
      modifiers: getModifiers(e),
    });
  }, [getVideoCoords, getModifiers, sendInput]);

  const handleWheel = useCallback((e: React.WheelEvent<HTMLVideoElement>) => {
    const { x, y } = getVideoCoords(e);
    void sendInput({
      type: 'mouse',
      eventType: 'mouseWheel',
      x,
      y,
      deltaX: e.deltaX,
      deltaY: e.deltaY,
      modifiers: getModifiers(e),
    });
  }, [getVideoCoords, getModifiers, sendInput]);

  const handleKeyDown = useCallback((e: React.KeyboardEvent<HTMLVideoElement>) => {
    e.preventDefault();
    void sendInput({
      type: 'keyboard',
      eventType: 'keyDown',
      key: e.key,
      code: e.code,
      text: e.key.length === 1 ? e.key : undefined,
      modifiers: getModifiers(e),
    });
  }, [getModifiers, sendInput]);

  const handleKeyUp = useCallback((e: React.KeyboardEvent<HTMLVideoElement>) => {
    e.preventDefault();
    void sendInput({
      type: 'keyboard',
      eventType: 'keyUp',
      key: e.key,
      code: e.code,
      modifiers: getModifiers(e),
    });
  }, [getModifiers, sendInput]);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
  }, []);

  if (!open) return null;

  return (
    <div className="flex h-full w-full min-w-0 flex-col border-l bg-background/95 backdrop-blur-xl">
      <div className="flex shrink-0 items-center justify-between border-b px-4 py-3">
        <div className="text-base font-semibold flex items-center gap-2">
          <Globe className="h-4 w-4" />
          Browser
          {connected && (
            <span className="text-xs text-green-500 font-normal">Connected</span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            {loading && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            <span>
              {!browserEnabled
                ? 'Disabled'
                : !running
                  ? 'Stopped'
                  : takeoverActive
                    ? 'You are in control'
                    : 'Managed by May'}
            </span>
          </div>
          {takeoverActive && (
            <Button
              variant="outline"
              size="sm"
              className="h-7 border-amber-500/50 text-amber-600 hover:bg-amber-500/10"
              onClick={() => void releaseTakeover()}
            >
              Return control
            </Button>
          )}
          <Button variant="ghost" size="icon" className="h-8 w-8" onClick={onClose}>
            <X className="h-4 w-4" />
          </Button>
        </div>
      </div>

      {running && (
        <div className="flex shrink-0 items-center gap-2 border-b px-4 py-2">
          <Input
            value={urlInput}
            onChange={(e) => setUrlInput(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && handleNavigate()}
            placeholder="Enter URL..."
            className="h-8 flex-1 text-sm"
          />
          <Button
            variant="ghost"
            size="icon"
            className="h-8 w-8"
            onClick={handleNavigate}
          >
            <ArrowRight className="h-4 w-4" />
          </Button>
        </div>
      )}

      {error && (
        <div className="bg-destructive/10 px-4 py-2 text-sm text-destructive">
          {error}
        </div>
      )}

      <div className="flex flex-1 items-center justify-center overflow-hidden bg-muted/50">
        {!running ? (
          <div className="text-center text-muted-foreground">
            <Globe className="mx-auto mb-3 h-12 w-12 opacity-30" />
            <p className="text-sm">
              {browserEnabled ? 'Browser is not running.' : 'Browser is disabled in settings.'}
            </p>
            <p className="mt-1 text-xs">
              {browserEnabled
                ? 'May manages the browser lifetime. Start it explicitly if you need a live session.'
                : 'Enable the browser feature in settings to let May provision and manage the runtime.'}
            </p>
            {browserEnabled && (
              <Button
                className="mt-4"
                onClick={() => void handleStartBrowser()}
                disabled={loading}
              >
                {loading && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                Start Browser
              </Button>
            )}
          </div>
        ) : (
          <div className="relative flex h-full w-full items-center justify-center">
            <video
              ref={videoRef}
              autoPlay
              playsInline
              muted={false}
              tabIndex={0}
              className={cn(
                'max-h-full max-w-full outline-none transition-shadow duration-200',
                takeoverActive
                  ? 'cursor-crosshair ring-2 ring-amber-500/60'
                  : 'cursor-default',
              )}
              onMouseDown={handleMouseDown}
              onMouseUp={handleMouseUp}
              onMouseMove={handleMouseMove}
              onWheel={handleWheel}
              onKeyDown={handleKeyDown}
              onKeyUp={handleKeyUp}
              onContextMenu={handleContextMenu}
            />
            {connected && !takeoverActive && (
              <button
                type="button"
                onClick={() => void acquireTakeover()}
                className="absolute inset-0 flex items-center justify-center bg-background/40 backdrop-blur-[1px] transition-colors hover:bg-background/50"
                aria-label="Take control of the browser"
              >
                <div className="flex items-center gap-2 rounded-full bg-background/80 px-4 py-2 text-sm font-medium shadow-lg ring-1 ring-border/60">
                  <MousePointerClick className="h-4 w-4" />
                  May is driving — click to take control
                </div>
              </button>
            )}
            {takeoverActive && (
              <div className="pointer-events-none absolute left-3 top-3 rounded-full bg-amber-500/90 px-2.5 py-1 text-[11px] font-medium text-amber-950 shadow">
                You are in control · Esc to release
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
