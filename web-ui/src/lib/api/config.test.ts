import { afterEach, describe, expect, mock, test } from 'bun:test';
import { setFullConfig, type UiFullConfig, type UiProviderConfig } from './config';

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function makeProvider(): UiProviderConfig {
  return {
    id: 'chatgpt',
    providerType: 'chatgpt',
    apiKey: null,
    baseUrl: null,
    displayName: null,
    enabled: true,
    nativeTools: { webSearch: true, webFetch: false },
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
