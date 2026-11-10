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
  /** Key material the user typed in the editor, or `null` to mark it removed; absent on reads and untouched drafts. */
  apiKey?: string | null;
  baseUrl?: string | null;
  displayName?: string | null;
  enabled: boolean;
  nativeTools: UiNativeToolsConfig;
  cacheLifetime: CacheLifetime;
  api?: ProviderApi | null;
}

/** What a config save does with a stored secret. */
export type SecretUpdate = 'keep' | 'remove' | { replace: string };

/** A provider as the save endpoint expects it: `apiKey` is a SecretUpdate. */
export type UiProviderSaveConfig = Omit<UiProviderConfig, 'apiKey'> & {
  apiKey?: SecretUpdate;
};

/** The save payload: providers carry SecretUpdate instead of raw key text. */
export type UiFullSaveConfig = Omit<UiFullConfig, 'providers'> & {
  providers: UiProviderSaveConfig[];
};

export interface UiModelsConfig {
  advanced: string[];
  balanced: string[];
  fast: string[];
  imageGeneration: string[];
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
  tailscaleBinary: string;
  httpsPort: number;
  allowLocalEnvironment: boolean;
  funnel: boolean;
  funnelPort: number;
  /** Largest webhook delivery body in bytes; larger deliveries get 413. */
  webhookMaxBodyBytes: number;
  /** Tailscale logins accepted besides the login owning this node, which is always accepted. */
  allowedLogins: string[];
}

export type CommandPriority = 'normal' | 'low' | 'lowest';

export interface UiSandboxConfig {
  commandPriority: CommandPriority;
  /** Jobserver tokens shared by every terminal; null is the host's available parallelism. */
  jobPoolSize: number | null;
}

export type StreamCodec = 'auto' | 'h264' | 'av1';

export interface UiStreamConfig {
  codec: StreamCodec;
}

export interface UiSuggestionsConfig {
  enabled: boolean;
}

export interface UiRecapConfig {
  auto: boolean;
  idleMinutes: number;
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
  stream: UiStreamConfig;
  /** Omitted from a save, the setting is left as it is. */
  suggestions?: UiSuggestionsConfig;
  recap?: UiRecapConfig;
  sessions?: UiSessionsConfig;
}

export interface UiSessionsConfig {
  /** Hours a session stays listed after it went idle. */
  recentHours: number;
}

export interface UiSshServerConfig {
  name: string;
  target: string;
  /** Kernel and architecture the server reported when it was added. */
  platform: { os: string; arch: string };
}

export interface UiConfigResponse {
  sshServers: UiSshServerConfig[];
  maxIterations: number;
  uiFont: string;
  uiTheme: string;
  /** IANA name of the user's timezone (`[time] timezone`). */
  timezone: string;
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

/**
 * `logins` with `input` appended: trimmed, and unchanged when blank or an
 * exact duplicate.
 */
export function addAllowedLogin(logins: string[], input: string): string[] {
  const login = input.trim();
  return login === '' || logins.includes(login) ? logins : [...logins, login];
}

/** `full` with the remote-access allow-list replaced. */
export function withAllowedLogins(full: UiFullConfig, allowedLogins: string[]): UiFullConfig {
  return { ...full, remoteAccess: { ...full.remoteAccess, allowedLogins } };
}

export function setFullConfig(full: UiFullConfig): Promise<void> {
  // The draft's raw key text becomes the save's SecretUpdate: untouched
  // fields keep the stored secret, typed text replaces it, a cleared field
  // or explicit null removes it.
  const body: UiFullSaveConfig = {
    ...full,
    providers: full.providers.map((provider) => {
      const apiKey = provider.apiKey;
      const update =
        apiKey === undefined
          ? undefined
          : apiKey && apiKey.trim()
            ? { replace: apiKey }
            : 'remove';
      return { ...provider, apiKey: update };
    }),
  };
  return apiPost<void>('/config/full', body);
}

export function upsertSshServer(server: Pick<UiSshServerConfig, 'name' | 'target'>): Promise<void> {
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
