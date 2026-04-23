import i18next from 'i18next';
import LanguageDetector from 'i18next-browser-languagedetector';
import { initReactI18next, useTranslation as useReactI18nTranslation } from 'react-i18next';
import { create } from 'zustand';
import { enUS, type Messages } from './locales/en-US';
import { zhHant } from './locales/zh-Hant';
import { ja } from './locales/ja';

export type Locale = 'en-US' | 'zh-Hant' | 'ja';

export const LOCALES: Array<{ id: Locale; label: string; nativeLabel: string }> = [
  { id: 'en-US', label: 'English (US)', nativeLabel: 'English (US)' },
  { id: 'zh-Hant', label: 'Traditional Chinese', nativeLabel: '繁體中文' },
  { id: 'ja', label: 'Japanese', nativeLabel: '日本語' },
];

const SUPPORTED: Locale[] = ['en-US', 'zh-Hant', 'ja'];

function normalizeLocale(raw: string | undefined): Locale {
  if (!raw) return 'en-US';
  const tag = raw.toLowerCase();
  if (
    tag.startsWith('zh-hant') ||
    tag.startsWith('zh-tw') ||
    tag.startsWith('zh-hk') ||
    tag.startsWith('zh-mo')
  ) {
    return 'zh-Hant';
  }
  if (tag.startsWith('ja')) return 'ja';
  if (tag.startsWith('en')) return 'en-US';
  if ((SUPPORTED as string[]).includes(raw)) return raw as Locale;
  return 'en-US';
}

if (!i18next.isInitialized) {
  i18next
    .use(LanguageDetector)
    .use(initReactI18next)
    .init({
      resources: {
        'en-US': { translation: enUS },
        'zh-Hant': { translation: zhHant },
        ja: { translation: ja },
      },
      supportedLngs: SUPPORTED,
      fallbackLng: 'en-US',
      detection: {
        order: ['localStorage', 'navigator'],
        lookupLocalStorage: 'may-i18n-locale',
        caches: ['localStorage'],
        convertDetectedLanguage: (lng: string) => normalizeLocale(lng),
      },
      interpolation: {
        escapeValue: false,
        prefix: '{',
        suffix: '}',
      },
      returnNull: false,
    });
}

type PathOf<T, Prefix extends string = ''> = {
  [K in keyof T & string]: T[K] extends string
    ? `${Prefix}${K}`
    : T[K] extends Record<string, unknown>
      ? PathOf<T[K], `${Prefix}${K}.`>
      : never;
}[keyof T & string];

export type MessageKey = PathOf<Messages>;

interface LocaleState {
  locale: Locale;
  setLocale: (l: Locale) => void;
}

export const useLocaleStore = create<LocaleState>((set) => ({
  locale: normalizeLocale(i18next.language),
  setLocale: (locale) => {
    void i18next.changeLanguage(locale);
    set({ locale });
  },
}));

i18next.on('languageChanged', (lng) => {
  const next = normalizeLocale(lng);
  if (useLocaleStore.getState().locale !== next) {
    useLocaleStore.setState({ locale: next });
  }
});

export function useTranslation() {
  const { t: rawT } = useReactI18nTranslation();
  const locale = useLocaleStore((s) => s.locale);
  const setLocale = useLocaleStore((s) => s.setLocale);
  const t = (key: MessageKey, vars?: Record<string, string | number>): string =>
    rawT(key, vars as Record<string, unknown>) as string;
  return { t, locale, setLocale };
}

export function translate(key: MessageKey, vars?: Record<string, string | number>): string {
  return i18next.t(key, vars as Record<string, unknown>) as string;
}

export { i18next };
