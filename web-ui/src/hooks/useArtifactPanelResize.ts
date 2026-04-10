import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ARTIFACT_PANEL_DEFAULT_WIDTH,
  ARTIFACT_PANEL_MAX_VIEWPORT_RATIO,
  ARTIFACT_PANEL_MIN_CHAT_WIDTH,
  ARTIFACT_PANEL_MIN_WIDTH,
} from '@/components/chat/chatUtils';

const DESKTOP_SIDEBAR_WIDTH = 256;

interface ResizeStartState {
  pointerId: number;
  x: number;
  width: number;
}

export interface UseArtifactPanelResizeInput {
  /** Whether the app-shell sidebar is currently expanded on desktop. */
  isSidebarOpen: boolean;
}

export interface UseArtifactPanelResizeResult {
  width: number;
  isResizing: boolean;
  onResizerPointerDown: (event: React.PointerEvent<HTMLDivElement>) => void;
}

/**
 * Owns the resizable artifact panel: clamps width to the valid range
 * based on viewport and sidebar state, handles the pointer-drag resize
 * lifecycle, and listens for viewport resize events so the panel
 * shrinks gracefully when the window gets narrower.
 */
export function useArtifactPanelResize({
  isSidebarOpen,
}: UseArtifactPanelResizeInput): UseArtifactPanelResizeResult {
  const [width, setWidth] = useState(ARTIFACT_PANEL_DEFAULT_WIDTH);
  const [isResizing, setIsResizing] = useState(false);
  const resizeStartRef = useRef<ResizeStartState | null>(null);

  const clampWidth = useCallback(
    (rawWidth: number): number => {
      if (typeof window === 'undefined') return rawWidth;
      const viewportWidth = window.innerWidth;
      const desktopSidebarWidth = isSidebarOpen ? DESKTOP_SIDEBAR_WIDTH : 0;
      const maxByViewport = Math.floor(
        viewportWidth * ARTIFACT_PANEL_MAX_VIEWPORT_RATIO,
      );
      const maxByChat =
        viewportWidth - desktopSidebarWidth - ARTIFACT_PANEL_MIN_CHAT_WIDTH;
      const maxWidth = Math.max(
        ARTIFACT_PANEL_MIN_WIDTH,
        Math.min(maxByViewport, maxByChat),
      );
      return Math.max(
        ARTIFACT_PANEL_MIN_WIDTH,
        Math.min(maxWidth, Math.round(rawWidth)),
      );
    },
    [isSidebarOpen],
  );

  const onResizerPointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      event.preventDefault();
      resizeStartRef.current = {
        pointerId: event.pointerId,
        x: event.clientX,
        width,
      };
      setIsResizing(true);
    },
    [width],
  );

  // Clamp width when the viewport resizes.
  useEffect(() => {
    const handleWindowResize = () => {
      setWidth((prev) => clampWidth(prev));
    };
    handleWindowResize();
    window.addEventListener('resize', handleWindowResize);
    return () => {
      window.removeEventListener('resize', handleWindowResize);
    };
  }, [clampWidth]);

  // Global pointer listeners while the user is dragging the resizer.
  useEffect(() => {
    if (!isResizing) return;

    const previousUserSelect = document.body.style.userSelect;
    const previousCursor = document.body.style.cursor;
    document.body.style.userSelect = 'none';
    document.body.style.cursor = 'col-resize';

    const handlePointerMove = (event: PointerEvent) => {
      const resizeState = resizeStartRef.current;
      if (!resizeState || event.pointerId !== resizeState.pointerId) return;
      if ((event.buttons & 1) === 0) {
        resizeStartRef.current = null;
        setIsResizing(false);
        return;
      }
      const delta = resizeState.x - event.clientX;
      setWidth(clampWidth(resizeState.width + delta));
    };

    const stopResizing = (event: PointerEvent) => {
      const resizeState = resizeStartRef.current;
      if (!resizeState || event.pointerId !== resizeState.pointerId) return;
      resizeStartRef.current = null;
      setIsResizing(false);
    };

    window.addEventListener('pointermove', handlePointerMove);
    window.addEventListener('pointerup', stopResizing);
    window.addEventListener('pointercancel', stopResizing);

    return () => {
      window.removeEventListener('pointermove', handlePointerMove);
      window.removeEventListener('pointerup', stopResizing);
      window.removeEventListener('pointercancel', stopResizing);
      document.body.style.userSelect = previousUserSelect;
      document.body.style.cursor = previousCursor;
    };
  }, [clampWidth, isResizing]);

  return { width, isResizing, onResizerPointerDown };
}
