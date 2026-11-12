import { describe, expect, test, afterEach } from 'bun:test';
import { translate, useLocaleStore } from './index';

const reset = () => useLocaleStore.getState().setLocale('en-US');

afterEach(reset);

describe('i18n', () => {
  test('resolves nested keys in en-US', () => {
    expect(translate('common.cancel')).toBe('Cancel');
    expect(translate('home.welcome')).toBe('Welcome to May');
  });

  test('switches to zh-Hant', () => {
    useLocaleStore.getState().setLocale('zh-Hant');
    expect(translate('common.cancel')).toBe('取消');
    expect(translate('home.welcome')).toBe('歡迎使用 May');
  });

  test('switches to ja', () => {
    useLocaleStore.getState().setLocale('ja');
    expect(translate('common.cancel')).toBe('キャンセル');
    expect(translate('home.welcome')).toBe('May へようこそ');
  });

  test('formats interpolation variables', () => {
    expect(translate('runtime.queued', { count: 3 })).toBe('3 queued');
    useLocaleStore.getState().setLocale('ja');
    expect(translate('runtime.queued', { count: 3 })).toBe('3 件キュー');
  });

  test('returns key path when missing', () => {
    expect(translate('common.cancel' as unknown as 'common.cancel')).toBeTruthy();
    // invalid path — should fall back to the path itself
    expect(translate('nope.nope' as unknown as 'common.cancel')).toBe('nope.nope');
  });
});
