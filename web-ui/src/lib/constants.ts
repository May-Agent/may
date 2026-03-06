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
  contextWindow?: number;
  abilities?: string[];
  reasoning?: boolean;
}

export const DEFAULT_MODELS: Model[] = [];

export const PROVIDER_ICONS: Record<Provider, ComponentType<{ className?: string }>> = {
  openai: OpenAI,
  anthropic: Anthropic,
  google: Google,
};

export function inferProvider(modelId: string): Provider {
  const id = modelId.toLowerCase();
  if (id.includes('claude')) return 'anthropic';
  if (id.includes('gemini')) return 'google';
  return 'openai';
}

export function buildModelCatalog(
  modelIds: string[],
  details?: Array<{ id: string; nickname: string; contextWindow?: number; abilities?: string[] }>,
): Model[] {
  if (!modelIds.length) return [];
  const detailsMap = new Map((details ?? []).map((d) => [d.id, d]));
  return modelIds.map((id) => {
    const meta = detailsMap.get(id);
    const abilities = meta?.abilities ?? [];
    return {
      id,
      name: meta?.nickname || id,
      provider: inferProvider(id),
      contextWindow: meta?.contextWindow,
      abilities,
      reasoning: abilities.includes('reasoning') || /o1|reason/i.test(id),
    };
  });
}

export function getModel(modelId: string, catalog: Model[]): Model | undefined {
  return catalog.find((m) => m.id === modelId);
}
