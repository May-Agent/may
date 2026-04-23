import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Plus } from 'lucide-react';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { useTranslation } from '@/lib/i18n';
import { useProfilesStore } from '@/lib/stores/profiles';
import { useActiveProfile, profilePath } from '@/lib/profile/active';
import { ENV_ICON, envKindOf, profileAccent, profileInitial } from '@/lib/profile/env';
import type { ProfileInfo } from '@/lib/api/profiles';
import { cn } from '@/lib/utils';
import { NewProfileDialog } from './NewProfileDialog';

export function ProfileSwitcher() {
  const { t } = useTranslation();
  const profiles = useProfilesStore((s) => s.profiles);
  const { profileId: activeId, profile: active } = useActiveProfile();
  const navigate = useNavigate();
  const [dialogOpen, setDialogOpen] = useState(false);

  const handleSwitch = (id: string) => {
    if (id === activeId) return;
    navigate(profilePath(id));
  };

  return (
    <div className="border-t border-border p-2.5">
      {active && <ActiveProfileLabel profile={active} />}
      <div className="flex flex-wrap items-center gap-2 py-1">
        {profiles.map((profile) => (
          <ProfileAvatar
            key={profile.id}
            profile={profile}
            active={profile.id === activeId}
            onSelect={() => handleSwitch(profile.id)}
          />
        ))}
        <button
          type="button"
          onClick={() => setDialogOpen(true)}
          aria-label={t('sidebar.newProfile')}
          className="flex size-7 shrink-0 items-center justify-center rounded-full border border-dashed border-border text-muted-foreground transition-colors hover:border-foreground hover:text-foreground"
        >
          <Plus className="size-3.5" />
        </button>
      </div>
      <NewProfileDialog open={dialogOpen} onOpenChange={setDialogOpen} />
    </div>
  );
}

function ActiveProfileLabel({ profile }: { profile: ProfileInfo }) {
  const { t } = useTranslation();
  const EnvIcon = ENV_ICON[envKindOf(profile.environment)];
  return (
    <div className="mb-1.5 flex items-center gap-1.5 text-[11px] text-muted-foreground">
      <span className="truncate font-medium text-foreground">{profile.name}</span>
      <span aria-hidden>·</span>
      <EnvIcon className="size-3 shrink-0" />
      <span className="truncate">
        {profile.environment.type === 'container'
          ? profile.environment.image || t('profile.environment')
          : envKindOf(profile.environment)}
      </span>
    </div>
  );
}

interface ProfileAvatarProps {
  profile: ProfileInfo;
  active: boolean;
  onSelect: () => void;
}

function ProfileAvatar({ profile, active, onSelect }: ProfileAvatarProps) {
  const { t } = useTranslation();
  const accent = profileAccent(profile.id);
  const label = profileInitial(profile.name);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={onSelect}
          aria-label={t('profile.switchTo', { name: profile.name })}
          aria-pressed={active}
          className={cn(
            'relative flex shrink-0 select-none items-center justify-center rounded-full text-[11px] font-medium transition-all',
            active ? 'mx-0.5 size-8 ring-2 ring-inset' : 'size-7 hover:scale-105',
          )}
          style={{
            background: accent.soft,
            color: accent.solid,
            ...(active ? ({ '--tw-ring-color': accent.solid } as Record<string, string>) : {}),
          }}
        >
          <span className="leading-none">{label}</span>
        </button>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        <div className="font-medium">{profile.name}</div>
        {profile.persona ? (
          <div className="mt-0.5 text-[11px] opacity-80 line-clamp-3">{profile.persona}</div>
        ) : null}
      </TooltipContent>
    </Tooltip>
  );
}
