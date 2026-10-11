import { describe, expect, test } from 'bun:test';

describe('artifact runtime bridge', () => {
  test('captures console.error and reports it with the generic artifact type', async () => {
    const source = await Bun.file(
      new URL('../../../src/texts/artifact_runtime_bridge.js', import.meta.url),
    ).text();
    const messages: unknown[] = [];
    const listeners = new Map<string, (event: Record<string, unknown>) => void>();
    const timers = new Map<number, () => void>();
    let nextTimer = 0;
    const window = {
      parent: { postMessage: (message: unknown) => messages.push(message) },
      addEventListener: (name: string, listener: (event: Record<string, unknown>) => void) => {
        listeners.set(name, listener);
      },
    };
    const document = {
      currentScript: {
        dataset: { artifactType: 'html', validationProfile: 'generic' },
      },
      readyState: 'complete',
      querySelectorAll: () => [],
      body: { innerText: '' },
    };
    const console = { error: (..._values: unknown[]) => undefined };
    const setTimeout = (callback: () => void) => {
      nextTimer += 1;
      timers.set(nextTimer, callback);
      return nextTimer;
    };
    const clearTimeout = (timer: number) => timers.delete(timer);
    const execute = new Function(
      'window',
      'document',
      'console',
      'setTimeout',
      'clearTimeout',
      source,
    );
    execute(window, document, console, setTimeout, clearTimeout);

    console.error('render failed');
    console.error('x'.repeat(2100));
    while (timers.size > 0) {
      const [timer, callback] = timers.entries().next().value as [number, () => void];
      timers.delete(timer);
      callback();
    }

    expect(messages).toHaveLength(1);
    const message = messages[0] as { diagnostics: string[] };
    expect(message.diagnostics[0]).toBe('console.error: render failed');
    expect(message.diagnostics[1]).toHaveLength(2000);
    expect(message.diagnostics[1]).toEndWith(' ... [truncated]');
    expect(listeners.has('error')).toBeTrue();
    expect(listeners.has('unhandledrejection')).toBeTrue();
  });
});
