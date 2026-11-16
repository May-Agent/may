import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { searchFetchLabel } from '../../lib/hostedLabel';
import { SearchRow } from './SearchRow';

describe('search and fetch rows', () => {
  test('labels are Searched "query" and Fetched url', () => {
    expect(searchFetchLabel('web_search', { query: 'Rust 异步 非同期 비동기' }, 'x')).toBe(
      'Searched "Rust 异步 非同期 비동기"',
    );
    expect(searchFetchLabel('web_fetch', { url: 'https://example.com/a' }, 'x')).toBe(
      'Fetched https://example.com/a',
    );
    expect(searchFetchLabel('web_search', {}, 'Searching the web')).toBe('Searching the web');
    expect(searchFetchLabel('terminal', {}, 'x')).toBeNull();
  });

  test('a finished search is one row with a check and nothing to unfold', () => {
    const markup = renderToStaticMarkup(
      <SearchRow
        tool={{ variant: 'web_search', status: 'done', result: { kind: 'text', text: 'hits' } }}
        label='Searched "may agent"'
      />,
    );
    expect(markup).toContain('data-search-row');
    expect(markup).toContain('Searched &quot;may agent&quot;');
    expect(markup).toContain('lucide-check');
    expect(markup).not.toContain('<details');
    expect(markup).not.toContain('<pre');
    expect(markup).not.toContain('hits');
  });

  test('a failed fetch is one row with a cross', () => {
    const markup = renderToStaticMarkup(
      <SearchRow
        tool={{ variant: 'web_fetch', status: 'done', result: { kind: 'error', message: 'boom' } }}
        label="Fetched https://example.com"
      />,
    );
    expect(markup).toContain('lucide-x');
    expect(markup).not.toContain('<details');
  });
});
