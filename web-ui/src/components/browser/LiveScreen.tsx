import { forwardRef, useEffect, useImperativeHandle, useRef, type VideoHTMLAttributes } from 'react';

interface LiveScreenProps extends VideoHTMLAttributes<HTMLVideoElement> {
  stream: MediaStream | null;
}

/** A muted, autoplaying video bound to the shared browser stream. */
export const LiveScreen = forwardRef<HTMLVideoElement | null, LiveScreenProps>(function LiveScreen(
  { stream, ...props },
  ref,
) {
  const inner = useRef<HTMLVideoElement | null>(null);
  useImperativeHandle(ref, () => inner.current as HTMLVideoElement, []);

  useEffect(() => {
    const video = inner.current;
    if (!video || video.srcObject === stream) return;
    video.srcObject = stream;
  }, [stream]);

  return <video ref={inner} autoPlay playsInline muted {...props} />;
});
