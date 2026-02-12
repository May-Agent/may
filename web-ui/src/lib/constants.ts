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
  kind?: 'llm' | 'image' | 'embedding' | 'reranker' | string;
  reasoning?: boolean;
  alwaysReasoning?: boolean;
  reasoningEfforts?: string[];
  defaultReasoningEffort?: string;
  adaptiveReasoning?: boolean;
}

export const DEFAULT_MODELS: Model[] = [
  { id: 'gpt-4', name: 'GPT-4', provider: 'openai' },
  { id: 'o1-preview', name: 'o1-preview', provider: 'openai', reasoning: true },
  { id: 'gpt-3.5-turbo', name: 'GPT-3.5 Turbo', provider: 'openai' },
  { id: 'claude-3-opus', name: 'Claude 3 Opus', provider: 'anthropic' },
  { id: 'claude-3-sonnet', name: 'Claude 3 Sonnet', provider: 'anthropic' },
  { id: 'gemini-pro', name: 'Gemini Pro', provider: 'google' },
];

export const PROVIDER_ICONS: Record<Provider, ComponentType<{ className?: string }>> = {
  openai: OpenAI,
  anthropic: Anthropic,
  google: Google,
};

export function inferProvider(modelId: string): Provider {
  const id = modelId.toLowerCase();
  if (id.includes('anthropic') || id.includes('claude')) return 'anthropic';
  if (id.includes('google') || id.includes('gemini')) return 'google';
  return 'openai';
}

const REASONING_ABILITY = 'reasoning';
const ALWAYS_REASONING_ABILITY = 'always_reasoning';

function includesAbility(abilities: string[] | undefined, target: string): boolean {
  return (abilities ?? []).some((a) => a.toLowerCase() === target);
}

export function buildModelCatalog(
  modelIds: string[],
  details: Array<{
    id: string;
    nickname: string;
    abilities: string[];
    kind?: string;
    provider?: string;
    reasoning_efforts?: string[];
    reasoning_effort_default?: string;
    adaptive_reasoning?: boolean;
  }> = [],
): Model[] {
  if (!modelIds.length) return DEFAULT_MODELS;

  const detailMap = new Map(details.map((d) => [d.id, d]));

  return modelIds.map((id) => {
    const detail = detailMap.get(id);
    const efforts = (detail?.reasoning_efforts ?? []).map((e) => e.toLowerCase());
    return {
      id,
      name: detail?.nickname || id,
      provider: (detail?.provider ? inferProvider(detail.provider) : inferProvider(id)),
      kind: detail?.kind,
      reasoning: includesAbility(detail?.abilities, REASONING_ABILITY),
      alwaysReasoning: includesAbility(detail?.abilities, ALWAYS_REASONING_ABILITY),
      reasoningEfforts: efforts,
      defaultReasoningEffort: detail?.reasoning_effort_default?.toLowerCase(),
      adaptiveReasoning: detail?.adaptive_reasoning,
    };
  });
}

export function getModel(modelId: string, catalog: Model[]): Model {
  return catalog.find((m) => m.id === modelId) || catalog[0] || DEFAULT_MODELS[0];
}
