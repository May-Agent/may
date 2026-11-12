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

/** Thinking-header duration: integer ms below 0.1s, one decimal below 1s, then `12s` / `1m 4s` / `1h 2m`. */
export function formatThoughtDuration(seconds: number): string {
  const value = Number.isFinite(seconds) && seconds > 0 ? seconds : 0;
  if (value < 0.1) {
    const ms = Math.round(value * 1000);
    if (ms < 100) return `${ms}ms`;
  }
  if (value < 1) {
    const tenths = Math.round(value * 10);
    if (tenths < 10) return `${Math.floor(tenths / 10)}.${tenths % 10}s`;
    return formatThoughtWholeSeconds(1);
  }
  return formatThoughtWholeSeconds(Math.round(value));
}

function formatThoughtWholeSeconds(total: number): string {
  const hour = 3600;
  const minute = 60;
  if (total >= hour) {
    const hours = Math.floor(total / hour);
    const minutes = Math.floor((total % hour) / minute);
    return minutes === 0 ? `${hours}h` : `${hours}h ${minutes}m`;
  }
  if (total >= minute) {
    const minutes = Math.floor(total / minute);
    const secs = total % minute;
    return secs === 0 ? `${minutes}m` : `${minutes}m ${secs}s`;
  }
  return `${total}s`;
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
