import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { MapPin } from 'lucide-react';
import { toast } from 'sonner';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Segmented } from '@/components/ui/segmented';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { useProfilesStore } from '@/lib/stores/profiles';
import { profilePath } from '@/lib/profile/active';
import { ENV_ICON } from '@/lib/profile/env';
import type { ProfileEnvironmentKind } from '@/lib/types';

interface NewProfileDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

interface DraftState {
  name: string;
  environment: ProfileEnvironmentKind;
  containerImage: string;
  persona: string;
  locationAware: boolean;
}

const EMPTY_DRAFT: DraftState = {
  name: '',
  environment: 'local',
  containerImage: '',
  persona: '',
  locationAware: false,
};

const ENV_OPTIONS: { value: ProfileEnvironmentKind; labelKey: MessageKey }[] = [
  { value: 'local', labelKey: 'settings.profiles_env.envLocal' },
  { value: 'container', labelKey: 'settings.profiles_env.envContainer' },
];

export function NewProfileDialog({ open, onOpenChange }: NewProfileDialogProps) {
  const { t } = useTranslation();
  const create = useProfilesStore((s) => s.create);
  const navigate = useNavigate();
  const [draft, setDraft] = useState<DraftState>(EMPTY_DRAFT);
  const [busy, setBusy] = useState(false);

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) setDraft(EMPTY_DRAFT);
    onOpenChange(nextOpen);
  };

  const set = <K extends keyof DraftState>(key: K, value: DraftState[K]) =>
    setDraft((prev) => ({ ...prev, [key]: value }));

  const handleSubmit = async () => {
    const name = draft.name.trim();
    if (!name) {
      toast.error(t('profile.newDialog.nameRequired'));
      return;
    }
    setBusy(true);
    try {
      const created = await create({
        name,
        environment: draft.environment,
        containerImage:
          draft.environment === 'container' ? draft.containerImage.trim() || null : null,
        persona: draft.persona.trim() || null,
        locationAware: draft.locationAware,
      });
      navigate(profilePath(created.id));
      handleOpenChange(false);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>{t('profile.newDialog.title')}</DialogTitle>
          <DialogDescription>{t('profile.newDialog.description')}</DialogDescription>
        </DialogHeader>

        <div className="mt-2 space-y-4">
          <div className="space-y-1.5">
            <Label>{t('profile.newDialog.nameLabel')}</Label>
            <Input
              autoFocus
              value={draft.name}
              onChange={(e) => set('name', e.target.value)}
              placeholder={t('profile.newDialog.namePlaceholder')}
            />
          </div>

          <div className="space-y-2">
            <Label>{t('settings.profiles_env.environment')}</Label>
            <Segmented<ProfileEnvironmentKind>
              size="sm"
              value={draft.environment}
              onValueChange={(v) => set('environment', v)}
              options={ENV_OPTIONS.map(({ value, labelKey }) => {
                const Icon = ENV_ICON[value];
                return {
                  value,
                  icon: <Icon className="size-3" />,
                  label: t(labelKey),
                };
              })}
            />
          </div>

          {draft.environment === 'container' && (
            <div className="space-y-1.5">
              <Label>{t('settings.profiles_env.containerImage')}</Label>
              <Input
                value={draft.containerImage}
                onChange={(e) => set('containerImage', e.target.value)}
                placeholder="ubuntu:24.04"
                className="font-mono"
              />
            </div>
          )}

          <div className="space-y-1.5">
            <Label>{t('settings.profiles_env.persona')}</Label>
            <Textarea
              value={draft.persona}
              onChange={(e) => set('persona', e.target.value)}
              placeholder={t('profile.newDialog.personaPlaceholder')}
              rows={3}
            />
          </div>

          <div className="flex items-center justify-between gap-3 rounded-md bg-muted/40 px-3 py-2">
            <div className="flex items-center gap-2">
              <MapPin className="size-4 text-muted-foreground" />
              <div>
                <div className="text-sm font-medium">
                  {t('settings.profiles_env.locationAware')}
                </div>
              </div>
            </div>
            <Switch
              checked={draft.locationAware}
              onCheckedChange={(v) => set('locationAware', v)}
            />
          </div>
        </div>

        <div className="mt-6 flex justify-end gap-2">
          <Button
            size="sm"
            variant="ghost"
            onClick={() => handleOpenChange(false)}
            disabled={busy}
          >
            {t('profile.newDialog.cancel')}
          </Button>
          <Button size="sm" onClick={() => void handleSubmit()} disabled={busy}>
            {t('profile.newDialog.create')}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
