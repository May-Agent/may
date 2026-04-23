import { Check } from 'lucide-react';
import { cn } from '@/lib/utils';
import { LOCALES, useTranslation, type Locale } from '@/lib/i18n';
import { SectionHeader } from './SectionHeader';

export function LanguageSection() {
  const { t, locale, setLocale } = useTranslation();

  return (
    <section>
      <SectionHeader title={t('settings.language.title')} description={t('settings.language.description')} />
      <ul className="space-y-1">
        {LOCALES.map((l) => {
          const active = l.id === locale;
          return (
            <li key={l.id}>
              <button
                type="button"
                onClick={() => setLocale(l.id as Locale)}
                className={cn(
                  'flex w-full items-center justify-between rounded-md border border-border px-3 py-2 text-sm transition-colors',
                  active ? 'border-primary bg-primary/5' : 'hover:bg-accent',
                )}
              >
                <div className="flex flex-col items-start">
                  <span className="font-medium">{l.nativeLabel}</span>
                  <span className="text-xs text-muted-foreground">{l.label}</span>
                </div>
                {active && <Check className="size-4 text-primary" />}
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
