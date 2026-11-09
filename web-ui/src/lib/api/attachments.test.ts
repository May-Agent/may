import { afterEach, describe, expect, mock, test } from 'bun:test';
import { uploadAttachment } from './artifacts';

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('uploadAttachment', () => {
  test('posts the file bytes with its name and content type', async () => {
    const record = { id: 'a1', name: 'shot.png', type: 'image', url: '/api/sessions/s/files/shot.png' } as const;
    const seen: { url: string; method?: string; type?: string; body: unknown } = { url: '', body: undefined };
    globalThis.fetch = mock(async (input: RequestInfo | URL, init?: RequestInit) => {
      seen.url = String(input);
      seen.method = init?.method;
      seen.type = (init?.headers as Record<string, string>)?.['content-type'];
      seen.body = init?.body;
      return new Response(JSON.stringify(record), { status: 200 });
    }) as unknown as typeof fetch;

    const file = new File(['PNG'], 'my shot.png', { type: 'image/png' });
    const result = await uploadAttachment('s', file);

    expect(result).toEqual(record);
    expect(seen?.url).toBe('/api/sessions/s/attachments?name=my%20shot.png');
    expect(seen?.method).toBe('POST');
    expect(seen?.type).toBe('image/png');
    expect(seen?.body).toBe(file);
  });
});
