import { useEffect, useState } from 'react';
import { useForm, useWatch } from 'react-hook-form';
import { zodResolver } from '@hookform/resolvers/zod';
import { z } from 'zod';
import { Download, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { installSkill, listSkills, uninstallSkill, type SkillInfo } from '@/lib/api/skills';

const schema = z.object({
  repo: z.string().trim().min(1),
});
type FormValues = z.infer<typeof schema>;

export function SkillsSection() {
  const { t } = useTranslation();
  const [items, setItems] = useState<SkillInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const form = useForm<FormValues>({
    resolver: zodResolver(schema),
    defaultValues: { repo: '' },
  });

  const refresh = async () => {
    setLoading(true);
    try {
      setItems(await listSkills());
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    let cancelled = false;
    void listSkills()
      .then((skills) => {
        if (!cancelled) setItems(skills);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const onSubmit = form.handleSubmit(async ({ repo }) => {
    try {
      await installSkill(repo);
      form.reset({ repo: '' });
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    }
  });

  const handleUninstall = async (dir: string) => {
    setBusy(true);
    try {
      await uninstallSkill(dir);
      await refresh();
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setBusy(false);
    }
  };

  const submitting = form.formState.isSubmitting;
  const repoValue = useWatch({ control: form.control, name: 'repo' });

  return (
    <section>
      <SectionHeader title={t('settings.skills.title')} description={t('settings.skills.description')} />

      <form onSubmit={onSubmit} className="mb-4 flex gap-2">
        <input
          {...form.register('repo')}
          placeholder={t('settings.skills.repoPlaceholder')}
          className="flex-1 rounded-md border border-border bg-background px-3 py-2 text-sm font-mono outline-none focus:ring-2 focus:ring-ring"
        />
        <Button type="submit" disabled={submitting || !repoValue?.trim()}>
          <Download className="size-4" />
          {t('settings.skills.install')}
        </Button>
      </form>

      {loading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : items.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('settings.skills.empty')}</div>
      ) : (
        <ul className="space-y-1">
          {items.map((s) => (
            <li
              key={s.dir}
              className="flex items-center justify-between rounded-md border border-border px-3 py-2 text-sm"
            >
              <div className="min-w-0">
                <div className="truncate font-medium">{s.name}</div>
                <div className="truncate text-xs text-muted-foreground">
                  {s.description ?? s.source ?? s.dir}
                </div>
              </div>
              <Button
                size="icon"
                variant="ghost"
                disabled={busy}
                onClick={() => handleUninstall(s.dir)}
                aria-label={t('common.remove')}
              >
                <Trash2 className="size-4" />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
