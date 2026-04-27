import { apiDelete, apiGet, apiPost } from './client';

export type ProviderType =
  | 'anthropic'
  | 'openai'
  | 'google'
  | 'copilot'
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

export interface UiProviderConfig {
  id: string;
  providerType: ProviderType;
  apiKey?: string | null;
  baseUrl?: string | null;
  displayName?: string | null;
  enabled: boolean;
}

export interface UiModelsConfig {
  advanced: string[];
  balanced: string[];
  fast: string[];
  imageGeneration: string[];
  embeddingModel?: string | null;
  embeddingDimensions?: number | null;
}

export interface UiPreferencesConfig {
  warnOnModelChange: boolean;
  showTokenUsage: boolean;
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

export interface UiFullConfig {
  providers: UiProviderConfig[];
  models: UiModelsConfig;
  preferences: UiPreferencesConfig;
  appearance: UiAppearanceConfig;
  lastModel?: string | null;
  maxIterations: number;
  browser: UiBrowserConfig;
  remoteAccess: UiRemoteAccessConfig;
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
