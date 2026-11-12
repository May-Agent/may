import { describe, expect, test } from 'bun:test';
import type { TurnToolState } from '../stream/reducer';
import { browserActivity, groupBrowserRuns, hostOf } from './activity';

function tool(args: unknown, patch: Partial<TurnToolState> = {}): TurnToolState {
  return {
    id: 't',
    name: 'browser',
    variant: 'browser',
    argumentsRaw: JSON.stringify(args),
    summary: {},
    status: 'done',
    seq: 0,
    ...patch,
  };
}

describe('browserActivity', () => {
  test('reads the page and the screenshot a read reports', () => {
    const a = browserActivity(
      tool(
        { command: 'read', url: 'https://example.com/pricing' },
        {
          result: {
            kind: 'text',
            text: 'The browser is running.\n\nPage: Pricing - Example (https://example.com/pricing)\nScreenshot saved to artifacts/browser/p.png (1280x800).\n',
          },
        },
      ),
    );
    expect(a.action).toBe('read');
    expect(a.screenshot).toBe(true);
    expect(a.status).toBe('done');
    expect(a.pageTitle).toBe('Pricing - Example');
    expect(a.pageUrl).toBe('https://example.com/pricing');
    expect(a.screenshotPath).toBe('browser/p.png');
  });

  test('a read without a screenshot and a streaming call', () => {
    const a = browserActivity(
      tool({ command: 'read', url: 'https://a.dev', screenshot: 'off' }, { status: 'streaming' }),
    );
    expect(a.screenshot).toBe(false);
    expect(a.status).toBe('running');
    expect(a.screenshotPath).toBeNull();
  });

  test('errors and other commands', () => {
    const a = browserActivity(tool({ command: 'open' }, { result: { kind: 'error', message: 'no chrome' } }));
    expect(a.action).toBe('open');
    expect(a.status).toBe('failed');
    expect(a.error).toBe('no chrome');
    expect(browserActivity(tool({ command: 'cdp-endpoint' })).action).toBe('connect');
    expect(browserActivity(tool({})).action).toBe('other');
  });

  test('falls back to the summary target for the URL', () => {
    const a = browserActivity({ ...tool({ command: 'read' }), summary: { target: 'https://b.dev' } });
    expect(a.url).toBe('https://b.dev');
  });
});

describe('hostOf', () => {
  test('host or the input', () => {
    expect(hostOf('https://example.com/x?y')).toBe('example.com');
    expect(hostOf('not a url')).toBe('not a url');
  });
});

describe('groupBrowserRuns', () => {
  test('joins consecutive browser items', () => {
    const groups = groupBrowserRuns(['b1', 'b2', 't', 'b3'], (s) => s.startsWith('b'));
    expect(groups).toEqual([
      { kind: 'browser', items: ['b1', 'b2'] },
      { kind: 'item', item: 't' },
      { kind: 'browser', items: ['b3'] },
    ]);
  });
});
