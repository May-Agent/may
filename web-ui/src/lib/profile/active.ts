import { useMemo } from 'react';
import { useParams } from 'react-router-dom';
import { useProfilesStore } from '@/lib/stores/profiles';
import type { ProfileInfo } from '@/lib/api/profiles';

export interface ActiveProfileContext {
  profileId: string | null;
  profile: ProfileInfo | null;
}

/**
 * Read the active profile id straight from the URL. The URL is the sole
 * source of truth — zustand no longer tracks an `activeProfileId`.
 */
export function useActiveProfile(): ActiveProfileContext {
  const { profileId } = useParams<{ profileId: string }>();
  const profiles = useProfilesStore((s) => s.profiles);
  return useMemo(() => {
    if (!profileId) return { profileId: null, profile: null };
    const profile = profiles.find((p) => p.id === profileId) ?? null;
    return { profileId, profile };
  }, [profileId, profiles]);
}

/** Build the URL for a profile's home page. */
export function profilePath(profileId: string): string {
  return `/p/${encodeURIComponent(profileId)}`;
}

/** Build the URL for a session under a profile. */
export function profileSessionPath(profileId: string, sessionId: string): string {
  return `/p/${encodeURIComponent(profileId)}/s/${encodeURIComponent(sessionId)}`;
}
