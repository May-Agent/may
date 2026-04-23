import { useEffect } from 'react';
import { Navigate } from 'react-router-dom';
import {
  readLastProfileId,
  resolveDefaultProfileId,
  useProfilesStore,
} from '@/lib/stores/profiles';
import { profilePath } from '@/lib/profile/active';

/**
 * Landing redirect: figure out which profile the user should see and
 * forward them to `/p/:profileId`. Shown at `/` and for unknown paths.
 */
export function ProfileRedirect() {
  const profiles = useProfilesStore((s) => s.profiles);
  const defaultProfileId = useProfilesStore((s) => s.defaultProfileId);
  const loading = useProfilesStore((s) => s.loading);

  const target = resolveDefaultProfileId(profiles, readLastProfileId(), defaultProfileId);

  useEffect(() => {
    // keep a hint so a subsequent cold load goes back to the same profile
  }, []);

  if (loading && profiles.length === 0) {
    return <div className="flex h-full w-full items-center justify-center text-muted-foreground">Loading…</div>;
  }

  if (!target) {
    return (
      <div className="flex h-full w-full items-center justify-center text-muted-foreground">
        No profile available.
      </div>
    );
  }

  return <Navigate to={profilePath(target)} replace />;
}
