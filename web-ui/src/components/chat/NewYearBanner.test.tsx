import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { NewYearBanner } from './NewYearBanner';
import { useLocaleStore } from '@/lib/i18n';

function render(instant: string, timeZone: string) {
  return renderToStaticMarkup(<NewYearBanner timeZone={timeZone} now={new Date(instant)} />);
}

describe('NewYearBanner', () => {
  test('renders on December 31 and January 1 and not on December 30 or January 2, in two zones', () => {
    const cases: Array<[string, string, boolean]> = [
      ['2026-12-30T12:00:00-05:00', 'America/New_York', false],
      ['2026-12-31T12:00:00-05:00', 'America/New_York', true],
      ['2027-01-01T12:00:00-05:00', 'America/New_York', true],
      ['2027-01-02T12:00:00-05:00', 'America/New_York', false],
      ['2026-12-30T12:00:00+09:00', 'Asia/Tokyo', false],
      ['2026-12-31T12:00:00+09:00', 'Asia/Tokyo', true],
      ['2027-01-01T12:00:00+09:00', 'Asia/Tokyo', true],
      ['2027-01-02T12:00:00+09:00', 'Asia/Tokyo', false],
    ];
    for (const [instant, zone, shown] of cases) {
      expect(render(instant, zone).includes('new-year-banner')).toBe(shown);
    }
  });

  test('shows the greeting in each locale', () => {
    for (const [locale, text] of [
      ['en-US', 'Happy New Year!'],
      ['ja', '新年あけましておめでとうございます！'],
      ['zh-Hant', '新年快樂！'],
    ] as const) {
      useLocaleStore.getState().setLocale(locale);
      expect(render('2026-12-31T12:00:00-05:00', 'America/New_York')).toContain(text);
    }
    useLocaleStore.getState().setLocale('en-US');
  });
});
