import { useEffect, useState } from 'react';
import { Save, CheckCircle2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { getMemory, saveMemory } from '@/lib/api/memory';

export function MemorySection() {
  const { t } = useTranslation();
  const [initial, setInitial] = useState('');
  const [content, setContent] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await getMemory();
        if (cancelled) return;
        setInitial(res.content);
        setContent(res.content);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const dirty = content !== initial;

  const onSave = async () => {
    setSaving(true);
    try {
      const res = await saveMemory(content);
      setInitial(res.content);
      setContent(res.content);
      setSavedAt(Date.now());
    } catch (err) {
      toast.error(t('errors.generic'), {
        description: err instanceof Error ? err.message : undefined,
      });
    } finally {
      setSaving(false);
    }
  };

  const showSaved = savedAt !== null && !dirty;

  return (
    <section>
      <SectionHeader
        title={t('settings.memory.title')}
        description={t('settings.memory.description')}
        action={
          <div className="flex items-center gap-2">
            {showSaved && (
              <span className="inline-flex items-center gap-1 text-[11px] text-emerald-600 dark:text-emerald-400">
                <CheckCircle2 className="size-3.5" />
                {t('settings.memory.saved')}
              </span>
            )}
            <Button size="sm" disabled={!dirty || saving} onClick={onSave}>
              <Save className="size-3.5" />
              {t('settings.memory.save')}
            </Button>
          </div>
        }
      />

      {loading ? (
        <div className="py-10 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : (
        <div className="space-y-2">
          <Textarea
            value={content}
            onChange={(e) => setContent(e.target.value)}
            placeholder={t('settings.memory.empty')}
            spellCheck={false}
            className="min-h-[320px] font-mono text-xs leading-relaxed"
          />
          <div className="flex items-center justify-between text-[11px] text-muted-foreground">
            <span>
              {content.length.toLocaleString()} {content.length === 1 ? 'character' : 'characters'}
            </span>
            {dirty && <span>· unsaved changes</span>}
          </div>
        </div>
      )}
    </section>
  );
}
