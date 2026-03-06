import type { Session, ProfileInfo } from '@/types';
import { ApiError } from '@/lib/api';
import * as api from '@/lib/api';
import type { AppState } from './index';

export const PREVIEW_MAX_LENGTH = 100;
export const TITLE_MAX_LENGTH = 30;
export const TASK_REFRESH_DELAY_MS = 2000;

let profileSessionsRequestSequence = 0;

export function sortSessions(sessions: Session[]): Session[] {
  return [...sessions].sort((a, b) => {
    const aHasMessages = (a.messages?.length ?? 0) > 0;
    const bHasMessages = (b.messages?.length ?? 0) > 0;

    if (aHasMessages !== bHasMessages) {
      return bHasMessages ? 1 : -1;
    }

    return (
      new Date(b.lastModified).getTime() - new Date(a.lastModified).getTime()
    );
  });
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim().length > 0) {
    return error.message;
  }
  return String(error);
}

export function apiErrorCodeFromError(error: unknown): string | null {
  return error instanceof ApiError ? error.code : null;
}

export function backendUnavailableReasonFromError(error: unknown): string | null {
  if (error instanceof ApiError && error.code === 'backend_unavailable') {
    return error.message;
  }
  return null;
}

export function isContainerTransientBlockedCode(code: string | null): boolean {
  return code === 'container_preparing';
}

export function isContainerProfile(
  profileId: string | null,
  profiles: ProfileInfo[],
): boolean {
  if (!profileId) return false;
  const profile = profiles.find((item) => item.id === profileId);
  return profile?.environment.type === 'container';
}

export async function fetchAndApplyProfileSessions(
  profileId: string,
  get: () => AppState,
  set: (partial: Partial<AppState>) => void,
  options?: api.FetchProfileSessionsOptions,
): Promise<void> {
  const requestId = ++profileSessionsRequestSequence;
  try {
    const ps = await api.fetchProfileSessions(profileId, options);
    if (requestId !== profileSessionsRequestSequence) return;
    if (get().currentProfileId !== profileId) return;
    set({
      profileSessions: ps,
      profileAccessBlockedReason: null,
      profileAccessBlockedCode: null,
      backendUnavailableReason: null,
    });
  } catch (error) {
    if (requestId !== profileSessionsRequestSequence) return;
    if (get().currentProfileId !== profileId) return;
    const backendReason = backendUnavailableReasonFromError(error);
    const errorCode = apiErrorCodeFromError(error);
    if (isContainerProfile(profileId, get().profiles)) {
      set({
        profileSessions: null,
        profileAccessBlockedReason: errorMessage(error),
        profileAccessBlockedCode: errorCode,
        backendUnavailableReason: backendReason,
      });
    } else {
      console.error('failed to load profile sessions', error);
      if (backendReason) {
        set({ backendUnavailableReason: backendReason });
      }
    }
  } finally {
    if (requestId === profileSessionsRequestSequence) {
      set({ profileSessionsLoading: false });
    }
  }
}
