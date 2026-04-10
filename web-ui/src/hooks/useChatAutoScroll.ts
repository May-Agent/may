import { useCallback, useEffect, useRef } from 'react';
import type { Message } from '@/types';

const USER_SCROLLED_UP_THRESHOLD_PX = 150;
const USER_RETURNED_TO_BOTTOM_THRESHOLD_PX = 30;

export interface UseChatAutoScrollInput {
  /** Current session id — scroll position resets when this changes. */
  currentSessionId: string | null;
  /** Whether the chat surface is rendering (e.g. not welcome / loading). */
  isChatSurfaceActive: boolean;
  /** Current visible messages — used as dependency triggers. */
  messages: Message[];
}

export interface UseChatAutoScrollResult {
  scrollContainerRef: React.RefObject<HTMLDivElement | null>;
  contentRef: React.RefObject<HTMLDivElement | null>;
  userScrolledUpRef: React.MutableRefObject<boolean>;
  /** Call from the scroll container onScroll handler. */
  onScroll: () => void;
}

/**
 * Keeps the chat scroll container pinned to the bottom during streaming
 * unless the user has intentionally scrolled up, in which case we back
 * off and only resume auto-scroll when they return near the bottom.
 *
 * Uses a ResizeObserver on the content so we follow the growing
 * transcript even between React renders, and debounces the initial
 * scroll-to-bottom with two requestAnimationFrames to let layout
 * settle before measuring.
 */
export function useChatAutoScroll({
  currentSessionId,
  isChatSurfaceActive,
  messages,
}: UseChatAutoScrollInput): UseChatAutoScrollResult {
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const userScrolledUpRef = useRef(false);
  const pendingInitialScrollRef = useRef(false);

  const scrollToBottom = useCallback(() => {
    const container = scrollContainerRef.current;
    if (!container) return;
    container.scrollTop = container.scrollHeight;
  }, []);

  // Reset scroll-state tracking when the session changes.
  useEffect(() => {
    userScrolledUpRef.current = false;
    pendingInitialScrollRef.current = currentSessionId != null;
  }, [currentSessionId]);

  // When the chat surface becomes active, snap to the bottom after
  // two RAF ticks so initial layout has settled.
  useEffect(() => {
    if (
      !currentSessionId ||
      !isChatSurfaceActive ||
      !pendingInitialScrollRef.current
    ) {
      return;
    }

    let rafA = 0;
    let rafB = 0;
    rafA = requestAnimationFrame(() => {
      scrollToBottom();
      rafB = requestAnimationFrame(() => {
        scrollToBottom();
        userScrolledUpRef.current = false;
        pendingInitialScrollRef.current = false;
      });
    });

    return () => {
      if (rafA) cancelAnimationFrame(rafA);
      if (rafB) cancelAnimationFrame(rafB);
    };
  }, [currentSessionId, isChatSurfaceActive, messages.length, scrollToBottom]);

  // Observe content size changes so the scroll stays pinned during
  // streaming output without needing a React re-render.
  useEffect(() => {
    const container = scrollContainerRef.current;
    const content = contentRef.current;
    if (!container || !content) return;

    const observer = new ResizeObserver(() => {
      if (userScrolledUpRef.current) return;
      scrollToBottom();
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [currentSessionId, scrollToBottom]);

  // Scroll on message updates (new message, new chunk, thinking change).
  const lastMessage = messages[messages.length - 1];
  useEffect(() => {
    if (userScrolledUpRef.current) return;
    requestAnimationFrame(scrollToBottom);
  }, [
    currentSessionId,
    messages.length,
    lastMessage?.content,
    lastMessage?.thinking,
    lastMessage?.isStreaming,
    lastMessage?.isThinking,
    scrollToBottom,
  ]);

  const onScroll = useCallback(() => {
    if (pendingInitialScrollRef.current) return;
    const el = scrollContainerRef.current;
    if (!el) return;
    const distFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (distFromBottom > USER_SCROLLED_UP_THRESHOLD_PX) {
      userScrolledUpRef.current = true;
    } else if (distFromBottom < USER_RETURNED_TO_BOTTOM_THRESHOLD_PX) {
      userScrolledUpRef.current = false;
    }
  }, []);

  return {
    scrollContainerRef,
    contentRef,
    userScrolledUpRef,
    onScroll,
  };
}
