import { OpenAI, Anthropic, Google } from '@lobehub/icons';
import type { ComponentType } from 'react';

export const FONTS = [
  { name: 'Inter', value: 'Inter', family: 'Inter, sans-serif' },
  { name: 'Roboto', value: 'Roboto', family: 'Roboto, sans-serif' },
  { name: 'Open Sans', value: 'Open Sans', family: '"Open Sans", sans-serif' },
  { name: 'Lato', value: 'Lato', family: 'Lato, sans-serif' },
  { name: 'Montserrat', value: 'Montserrat', family: 'Montserrat, sans-serif' },
  { name: 'Source Han Sans', value: 'Source Han Sans SC', family: '"Source Han Sans SC", "Noto Sans SC", sans-serif' },
  { name: 'Source Han Serif', value: 'Source Han Serif SC', family: '"Source Han Serif SC", "Noto Serif SC", serif' },
] as const;

export type Provider = 'openai' | 'anthropic' | 'google';

export interface Model {
  id: string;
  name: string;
  provider: Provider;
  reasoning?: boolean;
}

export const MODELS: Model[] = [
  { id: 'gpt-4', name: 'GPT-4', provider: 'openai' },
  { id: 'o1-preview', name: 'o1-preview', provider: 'openai', reasoning: true },
  { id: 'gpt-3.5-turbo', name: 'GPT-3.5 Turbo', provider: 'openai' },
  { id: 'claude-3-opus', name: 'Claude 3 Opus', provider: 'anthropic' },
  { id: 'claude-3-sonnet', name: 'Claude 3 Sonnet', provider: 'anthropic' },
  { id: 'gemini-pro', name: 'Gemini Pro', provider: 'google' },
];

export const PROVIDER_ICONS: Record<Provider, ComponentType<any>> = {
  openai: OpenAI,
  anthropic: Anthropic,
  google: Google,
};

export function getModel(modelId: string): Model {
  return MODELS.find(m => m.id === modelId) || MODELS[0];
}
