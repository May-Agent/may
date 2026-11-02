import { create } from 'zustand';

/**
 * Whether the current listener requires an identity we don't have. Set by
 * the API client and SSE stream on `401`; the App shell renders the sign-in
 * placeholder instead of the app when set. The local loopback listener never
 * returns 401, so this only fires for remote listeners.
 */
export const useAuthGate = create<{ required: boolean }>(() => ({ required: false }));

/** Record that a request was rejected for a missing identity. */
export function markAuthRequired() {
  useAuthGate.setState({ required: true });
}
