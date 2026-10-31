import { useCallback, useSyncExternalStore } from 'react';

/** Layout breakpoints, matching Tailwind's `md` and `xl`. */
export const SIDEBAR_COLUMN_QUERY = '(min-width: 768px)';
export const INSPECTOR_COLUMN_QUERY = '(min-width: 1280px)';

/** Whether `query` currently matches; re-renders when it flips. */
export function useMediaQuery(query: string): boolean {
  const subscribe = useCallback(
    (notify: () => void) => {
      if (typeof window === 'undefined' || !window.matchMedia) return () => undefined;
      const list = window.matchMedia(query);
      list.addEventListener('change', notify);
      return () => list.removeEventListener('change', notify);
    },
    [query],
  );
  return useSyncExternalStore(
    subscribe,
    () => (typeof window !== 'undefined' && window.matchMedia ? window.matchMedia(query).matches : true),
    () => true,
  );
}
