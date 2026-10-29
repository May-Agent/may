import { apiDelete, apiGet, apiPost } from './client';

export type ProviderType =
  | 'anthropic'
  | 'openai'
  | 'google'
  | 'copilot'
  | 'chatgpt'
  | 'openrouter'
  | 'deepseek'
  | 'ollama'
  | 'lmstudio'
  | 'groq'
  | 'together'
  | 'mistral'
  | 'xai'
  | 'custom';

export type NetworkPolicyMode = 'disabled' | 'restricted' | 'allow_list' | 'deny_list';
export type BrowserBackendKind = 'managed' | 'remote_cdp';

export interface UiNativeToolsConfig {
  webSearch: boolean;
  webFetch: boolean;
}

export type ProviderApi = 'chat-completions' | 'responses';

/**
 * A provider's default cache lifetime: a duration such as `1h` or `5m`, or
 * `permanent`. The server parses it; an unparsable value fails the save.
 */
export type CacheLifetime = string;

/** The lifetime a provider has when its config declares none. */
export const DEFAULT_CACHE_LIFETIME: CacheLifetime = '1h';

export interface UiProviderConfig {
  id: string;
  providerType: ProviderType;
  /** Whether the server holds a key for this provider. Never carries key material. */
  hasApiKey: boolean;
  /** New key material entered by the user; sent only on save, absent on reads. */
  apiKey?: string | null;
  baseUrl?: string | null;
  displayName?: string | null;
  enabled: boolean;
  nativeTools: UiNativeToolsConfig;
  cacheLifetime: CacheLifetime;
  api?: ProviderApi | null;
}

export interface UiModelsConfig {
  advanced: string[];
  balanced: string[];
  fast: string[];
  imageGeneration: string[];
  embeddingModel?: string | null;
  embeddingDimensions?: number | null;
  /** Model compaction runs on instead of the session model, as `provider::model`. */
  compactionModel?: string | null;
}

export interface UiPreferencesConfig {
  warnOnModelChange: boolean;
  showTokenUsage: boolean;
  showDecodeSpeed: boolean;
  networkPolicyMode: NetworkPolicyMode;
  networkPolicyHosts: string[];
}

export interface UiAppearanceConfig {
  font: string;
  theme: string;
}

export interface UiSystemProfileConfig {
  path: string;
  copyOnFirstUse: boolean;
}

export interface UiBrowserConfig {
  enabled: boolean;
  backend: BrowserBackendKind;
  port: number;
  remoteCdpEndpoint?: string | null;
  screencastJpegQuality: number;
  encoderInitialBitrateKbps: number;
  screencastMaxWidth: number;
  screencastMaxHeight: number;
  everyNthFrame: number;
  stealthEnabled: boolean;
  systemProfile?: UiSystemProfileConfig | null;
}

export interface UiRemoteAccessConfig {
  enabled: boolean;
  cloudflaredBinary: string;
  tunnelToken?: string | null;
  publicUrl?: string | null;
  allowLocalEnvironment: boolean;
}

export type CommandPriority = 'normal' | 'low' | 'lowest';

export interface UiSandboxConfig {
  commandPriority: CommandPriority;
}

export interface UiFullConfig {
  providers: UiProviderConfig[];
  models: UiModelsConfig;
  preferences: UiPreferencesConfig;
  appearance: UiAppearanceConfig;
  lastModel?: string | null;
  maxIterations: number;
  browser: UiBrowserConfig;
  remoteAccess: UiRemoteAccessConfig;
  sandbox: UiSandboxConfig;
}

export interface UiSshServerConfig {
  name: string;
  target: string;
}

export interface UiConfigResponse {
  sshServers: UiSshServerConfig[];
  maxIterations: number;
  uiFont: string;
  uiTheme: string;
  fullConfig: UiFullConfig;
}

export function getConfig(): Promise<UiConfigResponse> {
  return apiGet<UiConfigResponse>('/config');
}

/** `full` with one provider's cache lifetime replaced. */
export function withCacheLifetime(
  full: UiFullConfig,
  providerId: string,
  cacheLifetime: CacheLifetime,
): UiFullConfig {
  return {
    ...full,
    providers: full.providers.map((p) => (p.id === providerId ? { ...p, cacheLifetime } : p)),
  };
}

/** `full` with the compaction model replaced; `null` compacts on the session model. */
export function withCompactionModel(full: UiFullConfig, selector: string | null): UiFullConfig {
  return { ...full, models: { ...full.models, compactionModel: selector } };
}

export function setFullConfig(full: UiFullConfig): Promise<void> {
  return apiPost<void>('/config/full', full);
}

export function upsertSshServer(server: UiSshServerConfig): Promise<void> {
  return apiPost<void>('/ssh', server);
}

export function deleteSshServer(name: string): Promise<void> {
  return apiDelete<void>(`/ssh/${encodeURIComponent(name)}`);
}

/**
 * True iff at least one provider is enabled. Model choice is a per-session
 * concern handled inside the chat composer, not a global gate.
 */
export function isConfigUsable(cfg: UiConfigResponse | null): boolean {
  if (!cfg) return false;
  return cfg.fullConfig.providers.some((p) => p.enabled);
}
