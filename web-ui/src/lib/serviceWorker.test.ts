import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

type Handler = (event: unknown) => void;

interface Shown {
  title: string;
  options: {
    body: string;
    tag: string;
    renotify: boolean;
    data: { sessionId?: string; profileId?: string };
  };
}

function loadServiceWorker() {
  const handlers: Record<string, Handler> = {};
  const shown: Shown[] = [];
  const navigated: string[] = [];
  const opened: string[] = [];
  let windows: unknown[] = [];
  const scope = {
    addEventListener: (type: string, handler: Handler) => {
      handlers[type] = handler;
    },
    registration: {
      showNotification: (title: string, options: Shown['options']) => {
        shown.push({ title, options });
        return Promise.resolve();
      },
    },
    clients: {
      matchAll: () => Promise.resolve(windows),
      openWindow: (path: string) => {
        opened.push(path);
        return Promise.resolve();
      },
    },
  };
  const source = readFileSync(new URL('../../public/sw.js', import.meta.url), 'utf8');
  new Function('self', source)(scope);
  return {
    handlers,
    shown,
    navigated,
    opened,
    attach(client: unknown) {
      windows = [client];
    },
  };
}

function waitUntilOf() {
  const pending: Promise<unknown>[] = [];
  return { waitUntil: (p: Promise<unknown>) => pending.push(p), settled: () => Promise.all(pending) };
}

describe('service worker', () => {
  test('a pushed notification names the session and what is asked', async () => {
    const sw = loadServiceWorker();
    const event = {
      ...waitUntilOf(),
      data: {
        json: () => ({
          title: 'Fix the build',
          body: 'Approve running: make install',
          sessionId: 's1',
          profileId: 'p1',
        }),
      },
    };
    sw.handlers.push(event);
    await event.settled();
    expect(sw.shown).toEqual([
      {
        title: 'Fix the build',
        options: {
          body: 'Approve running: make install',
          tag: 's1',
          renotify: true,
          data: { sessionId: 's1', profileId: 'p1' },
        },
      },
    ]);
  });

  test('a click opens the session in a new window', async () => {
    const sw = loadServiceWorker();
    const event = {
      ...waitUntilOf(),
      notification: { close: () => undefined, data: { sessionId: 's 1', profileId: 'p1' } },
    };
    sw.handlers.notificationclick(event);
    await event.settled();
    expect(sw.opened).toEqual(['/p/p1/s/s%201']);
  });

  test('a click brings an open window to the session', async () => {
    const sw = loadServiceWorker();
    const focused: string[] = [];
    const client = {
      focus: () => {
        focused.push('focus');
        return Promise.resolve(client);
      },
      navigate: (path: string) => {
        sw.navigated.push(path);
        return Promise.resolve(client);
      },
    };
    sw.attach(client);
    const event = {
      ...waitUntilOf(),
      notification: { close: () => undefined, data: { sessionId: 's1', profileId: 'p1' } },
    };
    sw.handlers.notificationclick(event);
    await event.settled();
    expect(sw.navigated).toEqual(['/p/p1/s/s1']);
    expect(focused).toEqual(['focus']);
    expect(sw.opened).toEqual([]);
  });
});
