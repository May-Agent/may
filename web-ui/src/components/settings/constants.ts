import { OpenAI, Anthropic, Google } from '@lobehub/icons';
import type { UiProviderConfig, UiFullConfig } from '@/lib/api';
import type { TaskTriggerKind, TimeTriggerType } from '@/types';

export type SettingsSection =
  | 'appearance'
  | 'providers'
  | 'models'
  | 'preferences'
  | 'memory'
  | 'logs'
  | 'ssh'
  | 'mcp'
  | 'skills'
  | 'tasks'
  | 'browser'
  | 'remoteAccess'
  | 'danger';

export const SECTIONS: ReadonlyArray<{ id: SettingsSection; label: string }> = [
  { id: 'appearance', label: 'Appearance' },
  { id: 'providers', label: 'Providers' },
  { id: 'models', label: 'Models' },
  { id: 'preferences', label: 'Preferences' },
  { id: 'memory', label: 'Memory' },
  { id: 'logs', label: 'Logs' },
  { id: 'ssh', label: 'SSH' },
  { id: 'mcp', label: 'MCP' },
  { id: 'skills', label: 'Skills' },
  { id: 'tasks', label: 'Tasks' },
  { id: 'browser', label: 'Browser' },
  { id: 'remoteAccess', label: 'Remote Access' },
  { id: 'danger', label: 'Danger Zone' },
];

export const PROVIDER_TYPES: UiProviderConfig['providerType'][] = [
  'anthropic',
  'openai',
  'google',
  'copilot',
  'openrouter',
  'deepseek',
  'ollama',
  'lmstudio',
  'groq',
  'together',
  'mistral',
  'xai',
  'custom',
];

export type TierKey = 'advanced' | 'balanced' | 'fast' | 'imageGeneration';

export const TIER_LABELS: Record<TierKey, string> = {
  advanced: 'Advanced',
  balanced: 'Balanced',
  fast: 'Fast',
  imageGeneration: 'Image generation',
};

export const PROVIDER_TYPE_LABEL: Record<UiProviderConfig['providerType'], string> = {
  anthropic: 'Anthropic',
  openai: 'OpenAI',
  google: 'Google',
  copilot: 'Copilot',
  openrouter: 'OpenRouter',
  deepseek: 'DeepSeek',
  ollama: 'Ollama',
  lmstudio: 'LM Studio',
  groq: 'Groq',
  together: 'Together',
  mistral: 'Mistral',
  xai: 'xAI',
  custom: 'Custom',
};

export const TASK_TRIGGER_KINDS: ReadonlyArray<{ value: TaskTriggerKind; label: string }> = [
  { value: 'time', label: 'Time' },
  { value: 'event', label: 'Event' },
  { value: 'webhook', label: 'Webhook' },
];

export const TIME_TRIGGER_TYPES: ReadonlyArray<{ value: TimeTriggerType; label: string }> = [
  { value: 'once', label: 'Once (UTC)' },
  { value: 'cron', label: 'Cron (UTC)' },
];

export function providerIcon(provider: UiProviderConfig) {
  switch (provider.providerType) {
    case 'anthropic':
      return Anthropic;
    case 'google':
      return Google;
    default:
      return OpenAI;
  }
}

export interface ConfigSectionProps {
  configDraft: UiFullConfig | null;
  setDraft: (updater: (prev: UiFullConfig) => UiFullConfig) => void;
}
