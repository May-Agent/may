import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';
import { formatDistanceToNow, type Locale as DateFnsLocale } from 'date-fns';
import { enUS as dfEnUS } from 'date-fns/locale/en-US';
import { zhTW as dfZhTW } from 'date-fns/locale/zh-TW';
import { ja as dfJa } from 'date-fns/locale/ja';
import { useLocaleStore, type Locale } from './i18n';

const DATE_FNS_LOCALES: Record<Locale, DateFnsLocale> = {
  'en-US': dfEnUS,
  'zh-Hant': dfZhTW,
  ja: dfJa,
};

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}

export function formatDurationMs(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '—';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  const remSeconds = Math.round(seconds % 60);
  return `${minutes}m ${remSeconds}s`;
}

function toDate(value: string | number | Date): Date | null {
  const date = typeof value === 'string' || typeof value === 'number' ? new Date(value) : value;
  return Number.isNaN(date.getTime()) ? null : date;
}

export function formatRelativeTime(value: string | number | Date, locale?: Locale): string {
  const date = toDate(value);
  if (!date) return '';
  const active = locale ?? useLocaleStore.getState().locale;
  return formatDistanceToNow(date, { addSuffix: true, locale: DATE_FNS_LOCALES[active] });
}

export function useFormatRelativeTime(): (value: string | number | Date) => string {
  const locale = useLocaleStore((s) => s.locale);
  return (value) => formatRelativeTime(value, locale);
}
