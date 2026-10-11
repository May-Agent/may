import { useEffect, useState } from 'react';
import {
  Plus,
  Trash2,
  ChevronRight,
  ChevronDown,
  MapPin,
  Save,
} from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Segmented } from '@/components/ui/segmented';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import {
  createProfile,
  deleteProfile,
  getProfileRemoteAttachments,
  listProfiles,
  setProfileRemoteAttachments,
  updateProfile,
  type ProfileInfo,
} from '@/lib/api/profiles';
import type { ProfileEnvironmentKind } from '@/lib/types';
import { ENV_ICON, envImageOf, envKindOf } from '@/lib/profile/env';
import { useConfigStore } from '@/lib/stores/config';

export function ProfilesSection() {
  const { t } = useTranslation();
  const [profiles, setProfiles] = useState<ProfileInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState<EditState | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setProfiles(await listProfiles());
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const startNew = () => {
    setDraft({
      mode: 'create',
      name: '',
      environment: 'local',
      containerImage: '',
      persona: '',
      locationAware: false,
    });
  };

  const save = async () => {
    if (!draft) return;
    if (!draft.name.trim()) {
      toast.error('Please name your profile');
      return;
    }
    setBusy(true);
    try {
      if (draft.mode === 'create') {
        await createProfile({
          name: draft.name.trim(),
          environment: draft.environment,
          containerImage: draft.environment === 'container' ? draft.containerImage.trim() || null : null,
          persona: draft.persona.trim() || null,
          locationAware: draft.locationAware,
        });
      } else {
        await updateProfile(draft.id, {
          name: draft.name.trim() || null,
          environment: draft.environment,
          containerImage: draft.environment === 'container' ? draft.containerImage.trim() || null : null,
          persona: draft.persona.trim() || null,
          locationAware: draft.locationAware,
        });
      }
      setDraft(null);
      setExpanded(null);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const del = async (id: string) => {
    setBusy(true);
    try {
      await deleteProfile(id);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const openEdit = (p: ProfileInfo) => {
    if (expanded === p.id) {
      setExpanded(null);
      setDraft(null);
      return;
    }
    setExpanded(p.id);
    setDraft({
      mode: 'edit',
      id: p.id,
      name: p.name,
      environment: envKindOf(p.environment),
      containerImage: envImageOf(p.environment),
      persona: p.persona ?? '',
      locationAware: p.locationAware,
    });
  };

  return (
    <section>
      <SectionHeader
        title={t('settings.profiles.title')}
        description={t('settings.profiles.description')}
        action={
          !draft || draft.mode !== 'create' ? (
            <Button size="sm" onClick={startNew}>
              <Plus className="size-3.5" />
              {t('settings.profiles.create')}
            </Button>
          ) : undefined
        }
      />

      <div className="space-y-2">
        {draft?.mode === 'create' && (
          <EditCard draft={draft} onChange={setDraft} onSave={save} onCancel={() => setDraft(null)} busy={busy} />
        )}

        {loading ? (
          <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
        ) : profiles.length === 0 && !draft ? (
          <div className="py-10 text-center text-sm text-muted-foreground">
            {t('settings.profiles.empty')}
          </div>
        ) : (
          <ul className="space-y-2">
            {profiles.map((p) => {
              const kind = envKindOf(p.environment);
              const Icon = ENV_ICON[kind];
              const isOpen = expanded === p.id;
              return (
                <li key={p.id} className="rounded-lg border border-border bg-card/20">
                  <div className="flex items-center gap-3 px-4 py-3">
                    <button
                      type="button"
                      onClick={() => openEdit(p)}
                      className="flex min-w-0 flex-1 items-center gap-3 text-left"
                    >
                      {isOpen ? (
                        <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
                      ) : (
                        <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
                      )}
                      <div className="flex size-8 shrink-0 items-center justify-center rounded-md bg-muted/40 text-base">
                        <Icon className="size-4 text-muted-foreground" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-sm font-medium">{p.name}</div>
                        <div className="truncate text-[11px] text-muted-foreground">
                          {kind === 'local' && 'Local · runs on this machine'}
                          {kind === 'container' && (
                            <>Container · {envImageOf(p.environment) || 'no image set'}</>
                          )}
                          {p.locationAware && (
                            <>
                              <span className="mx-1">·</span>
                              <MapPin className="inline size-3 align-middle" />
                              <span className="ml-0.5">location-aware</span>
                            </>
                          )}
                        </div>
                      </div>
                    </button>
                    <Button
                      size="icon"
                      variant="ghost"
                      disabled={busy}
                      onClick={() => del(p.id)}
                      aria-label={t('common.delete')}
                    >
                      <Trash2 className="size-4" />
                    </Button>
                  </div>
                  {isOpen && draft?.mode === 'edit' && draft.id === p.id && (
                    <div className="border-t border-border px-4 py-4">
                      <EditCard
                        draft={draft}
                        onChange={setDraft}
                        onSave={save}
                        onCancel={() => {
                          setDraft(null);
                          setExpanded(null);
                        }}
                        busy={busy}
                        inline
                      />
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}

type EditState =
  | {
      mode: 'create';
      name: string;
      environment: ProfileEnvironmentKind;
      containerImage: string;
      persona: string;
      locationAware: boolean;
    }
  | {
      mode: 'edit';
      id: string;
      name: string;
      environment: ProfileEnvironmentKind;
      containerImage: string;
      persona: string;
      locationAware: boolean;
    };

interface EditCardProps {
  draft: EditState;
  onChange: (next: EditState) => void;
  onSave: () => void;
  onCancel: () => void;
  busy: boolean;
  inline?: boolean;
}

function EditCard({ draft, onChange, onSave, onCancel, busy, inline }: EditCardProps) {
  const { t } = useTranslation();
  const set = <K extends keyof EditState>(key: K, value: EditState[K]) =>
    onChange({ ...draft, [key]: value });

  return (
    <div className={inline ? '' : 'rounded-lg border border-primary/30 bg-card/40 p-4 shadow-sm'}>
      <div className="space-y-1.5">
        <Label>Name</Label>
        <Input
          autoFocus={!inline}
          value={draft.name}
          onChange={(e) => set('name', e.target.value)}
          placeholder="Personal"
        />
      </div>

      <div className="mt-4 space-y-2">
        <Label>{t('settings.profiles_env.environment')}</Label>
        <Segmented<ProfileEnvironmentKind>
          size="sm"
          value={draft.environment}
          onValueChange={(v) => set('environment', v)}
          options={[
            { value: 'local' as const, labelKey: 'settings.profiles_env.envLocal' as const },
            { value: 'container' as const, labelKey: 'settings.profiles_env.envContainer' as const },
          ].map(({ value, labelKey }) => {
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
        <div className="mt-3 space-y-1.5">
          <Label>{t('settings.profiles_env.containerImage')}</Label>
          <Input
            value={draft.containerImage}
            onChange={(e) => set('containerImage', e.target.value)}
            placeholder="ubuntu:24.04"
            className="font-mono"
          />
        </div>
      )}

      <div className="mt-4 space-y-1.5">
        <Label>{t('settings.profiles_env.persona')}</Label>
        <Textarea
          value={draft.persona}
          onChange={(e) => set('persona', e.target.value)}
          placeholder="Any extra guidance for May in this profile — tone, focus areas, constraints…"
          rows={4}
        />
      </div>

      <div className="mt-4 flex items-center justify-between gap-3 rounded-md bg-muted/40 px-3 py-2">
        <div className="flex items-center gap-2">
          <MapPin className="size-4 text-muted-foreground" />
          <div>
            <div className="text-sm font-medium">{t('settings.profiles_env.locationAware')}</div>
            <div className="text-[11px] text-muted-foreground">
              May can ask the browser for your current location when relevant.
            </div>
          </div>
        </div>
        <Switch
          checked={draft.locationAware}
          onCheckedChange={(v) => set('locationAware', v)}
        />
      </div>

      {draft.mode === 'edit' && <RemoteAttachments profileId={draft.id} />}

      <div className="mt-4 flex justify-end gap-2">
        <Button size="sm" variant="ghost" onClick={onCancel} disabled={busy}>
          {t('common.cancel')}
        </Button>
        <Button size="sm" onClick={onSave} disabled={busy}>
          <Save className="size-3.5" />
          {t('common.save')}
        </Button>
      </div>
    </div>
  );
}

interface RemoteAttachmentsProps {
  profileId: string;
}

function RemoteAttachments({ profileId }: RemoteAttachmentsProps) {
  const { t } = useTranslation();
  const config = useConfigStore((s) => s.config);
  const servers = config?.sshServers ?? [];
  const [attached, setAttached] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void getProfileRemoteAttachments(profileId).then((names) => {
      if (!cancelled) setAttached(names);
    });
    return () => {
      cancelled = true;
    };
  }, [profileId]);

  const toggle = async (name: string, next: boolean) => {
    if (!attached) return;
    const updated = next
      ? [...attached, name]
      : attached.filter((n) => n !== name);
    setBusy(true);
    const previous = attached;
    setAttached(updated);
    try {
      await setProfileRemoteAttachments(profileId, updated);
    } catch (err) {
      setAttached(previous);
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mt-4 space-y-2 rounded-md border border-border bg-muted/20 px-3 py-2.5">
      <div>
        <div className="text-sm font-medium">{t('profile.remoteAttachments.title')}</div>
        <div className="mt-0.5 text-[11px] text-muted-foreground">
          {t('profile.remoteAttachments.description')}
        </div>
      </div>
      {servers.length === 0 ? (
        <div className="text-[11px] text-muted-foreground">
          {t('profile.remoteAttachments.addFromSettings')}
        </div>
      ) : (
        <ul className="space-y-1">
          {servers.map((server) => {
            const isAttached = attached?.includes(server.name) ?? false;
            return (
              <li
                key={server.name}
                className="flex items-center justify-between gap-3 rounded-md px-2 py-1.5"
              >
                <div className="min-w-0">
                  <div className="truncate text-sm">{server.name}</div>
                  <div className="truncate font-mono text-[11px] text-muted-foreground">
                    {server.target}
                  </div>
                </div>
                <Switch
                  checked={isAttached}
                  disabled={busy || attached === null}
                  onCheckedChange={(v) => void toggle(server.name, v)}
                />
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
