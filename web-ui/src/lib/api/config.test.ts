import { afterEach, describe, expect, mock, test } from 'bun:test';
import {
  DEFAULT_CACHE_LIFETIME,
  setFullConfig,
  withCacheLifetime,
  withCompactionModel,
  type UiFullConfig,
  type UiProviderConfig,
} from './config';

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function makeProvider(): UiProviderConfig {
  return {
    id: 'chatgpt',
    providerType: 'chatgpt',
    hasApiKey: false,
    baseUrl: null,
    displayName: null,
    enabled: true,
    nativeTools: { webSearch: true, webFetch: false },
    cacheLifetime: DEFAULT_CACHE_LIFETIME,
  };
}

function makeConfig(): UiFullConfig {
  return {
    providers: [makeProvider()],
    models: {
      advanced: [],
      balanced: [],
      fast: [],
      imageGeneration: [],
      embeddingModel: null,
      embeddingDimensions: null,
    },
    preferences: {
      warnOnModelChange: true,
      showTokenUsage: true,
      showDecodeSpeed: true,
      networkPolicyMode: 'disabled',
      networkPolicyHosts: [],
    },
    appearance: { font: 'Inter', theme: 'system' },
    lastModel: null,
    maxIterations: 0,
    browser: {
      enabled: false,
      backend: 'managed',
      port: 39222,
      remoteCdpEndpoint: null,
      screencastJpegQuality: 80,
      encoderInitialBitrateKbps: 4000,
      screencastMaxWidth: 1920,
      screencastMaxHeight: 1200,
      everyNthFrame: 2,
      stealthEnabled: true,
      systemProfile: null,
    },
    remoteAccess: {
      enabled: false,
      cloudflaredBinary: 'cloudflared',
      tunnelToken: null,
      publicUrl: null,
      allowLocalEnvironment: false,
    },
    sandbox: { commandPriority: 'low' },
  };
}

describe('setFullConfig', () => {
  test('posts provider nativeTools and chatgpt type unchanged', async () => {
    let capturedBody: unknown;
    let capturedUrl: unknown;
    globalThis.fetch = mock(async (input: RequestInfo | URL, init?: RequestInit) => {
      capturedUrl = input;
      capturedBody = JSON.parse(init?.body as string);
      return new Response('{}', { status: 200 });
    }) as unknown as typeof fetch;

    await setFullConfig(makeConfig());

    expect(capturedUrl).toBe('/api/config/full');
    const body = capturedBody as { providers: Array<Record<string, unknown>> };
    expect(body.providers).toHaveLength(1);
    expect(body.providers[0].providerType).toBe('chatgpt');
    expect(body.providers[0].nativeTools).toEqual({
      webSearch: true,
      webFetch: false,
    });
  });
});

describe('cache lifetime and compaction model saves', () => {
  async function savedBody(full: UiFullConfig) {
    let captured: unknown;
    globalThis.fetch = mock(async (_input: RequestInfo | URL, init?: RequestInit) => {
      captured = JSON.parse(init?.body as string);
      return new Response('{}', { status: 200 });
    }) as unknown as typeof fetch;
    await setFullConfig(full);
    return captured as {
      providers: Array<{ cacheLifetime: string }>;
      models: { compactionModel?: string | null };
    };
  }

  test('posts the default, a duration and permanent unchanged', async () => {
    const base = makeConfig();
    expect((await savedBody(base)).providers[0].cacheLifetime).toBe('1h');
    expect(
      (await savedBody(withCacheLifetime(base, 'chatgpt', '90m'))).providers[0].cacheLifetime,
    ).toBe('90m');
    expect(
      (await savedBody(withCacheLifetime(base, 'chatgpt', 'permanent'))).providers[0].cacheLifetime,
    ).toBe('permanent');
  });

  test('posts the compaction model and clears it with null', async () => {
    const base = makeConfig();
    const set = await savedBody(withCompactionModel(base, 'chatgpt::gpt-5-mini'));
    expect(set.models.compactionModel).toBe('chatgpt::gpt-5-mini');
    const cleared = await savedBody(withCompactionModel(base, null));
    expect(cleared.models.compactionModel).toBeNull();
  });
});
