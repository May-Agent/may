import { useCallback, useEffect, useMemo, useState } from 'react';
import { SlidersHorizontal, Trash2, ArrowUp, ArrowDown } from 'lucide-react';
import * as api from '@/lib/api';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useStore } from '@/lib/store';
import type { ProviderModelsItem } from '@/lib/api';
import { TIER_LABELS, type TierKey, type ConfigSectionProps } from './constants';

export function ModelsSection({ configDraft, setDraft }: ConfigSectionProps) {
  const { models, refreshModelsFromProviders } = useStore();
  const [modelSearch, setModelSearch] = useState('');
  const [modelsFetchLoading, setModelsFetchLoading] = useState(false);
  const [modelsFetchMessage, setModelsFetchMessage] = useState<string | null>(null);

  const fetchProviderModelsWithRetry = useCallback(async (): Promise<ProviderModelsItem[]> => {
    const maxAttempts = 4;
    for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
      const rows = await api.fetchModelsByProvider() as ProviderModelsItem[];
      if (rows.length > 0 || attempt === maxAttempts - 1) {
        return rows;
      }
      await new Promise<void>((resolve) => {
        window.setTimeout(resolve, 200);
      });
    }
    return [];
  }, []);

  // Load provider models on mount or when providers change
  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        await fetchProviderModelsWithRetry();
      } catch {
        // ignore
      }
      if (!cancelled) {
        // trigger a refresh of the store models so allModelIds stays up to date
      }
    };
    void load();
    return () => { cancelled = true; };
  }, [configDraft?.providers.length, fetchProviderModelsWithRetry]);

  const allModelIds = useMemo(() => {
    const ids = new Set(models.map((m) => m.id));
    if (configDraft) {
      configDraft.models.advanced.forEach((id) => ids.add(id));
      configDraft.models.balanced.forEach((id) => ids.add(id));
      configDraft.models.fast.forEach((id) => ids.add(id));
      configDraft.models.imageGeneration.forEach((id) => ids.add(id));
    }
    return Array.from(ids).sort((a, b) => a.localeCompare(b));
  }, [models, configDraft]);

  const filteredModelPool = useMemo(() => {
    const q = modelSearch.trim().toLowerCase();
    if (!q) return allModelIds;
    return allModelIds.filter((id) => id.toLowerCase().includes(q));
  }, [allModelIds, modelSearch]);

  const addModelToTier = (tier: TierKey, modelId: string) => {
    setDraft((prev) => {
      const current = prev.models[tier];
      if (current.includes(modelId)) return prev;
      return {
        ...prev,
        models: {
          ...prev.models,
          [tier]: [...current, modelId],
          modelProviders: {
            ...prev.models.modelProviders,
            [modelId]: prev.models.modelProviders[modelId] ?? '',
          },
        },
      };
    });
  };

  const removeModelFromTier = (tier: TierKey, modelId: string) => {
    setDraft((prev) => ({
      ...prev,
      models: {
        ...prev.models,
        [tier]: prev.models[tier].filter((id) => id !== modelId),
      },
    }));
  };

  const moveModelInTier = (tier: TierKey, index: number, dir: -1 | 1) => {
    setDraft((prev) => {
      const list = [...prev.models[tier]];
      const next = index + dir;
      if (next < 0 || next >= list.length) return prev;
      const [item] = list.splice(index, 1);
      list.splice(next, 0, item);
      return {
        ...prev,
        models: {
          ...prev.models,
          [tier]: list,
        },
      };
    });
  };

  const handleFetchModels = async () => {
    try {
      setModelsFetchLoading(true);
      setModelsFetchMessage(null);
      await refreshModelsFromProviders();
      await fetchProviderModelsWithRetry();
      setModelsFetchMessage('Models refreshed from providers');
    } catch (e) {
      setModelsFetchMessage((e as Error).message);
    } finally {
      setModelsFetchLoading(false);
    }
  };

  if (!configDraft) {
    return <div className="text-sm text-muted-foreground">Loading model config...</div>;
  }

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <SlidersHorizontal className="h-4 w-4" /> Models
      </div>

      <div className="space-y-4">
        <div className="space-y-1">
          <div className="flex items-center justify-between">
            <Label>Model Pool</Label>
            <Button size="sm" variant="outline" onClick={() => void handleFetchModels()} disabled={modelsFetchLoading}>
              {modelsFetchLoading ? 'Fetching\u2026' : 'Fetch models'}
            </Button>
          </div>
          <Input
            value={modelSearch}
            onChange={(e) => setModelSearch(e.target.value)}
            placeholder="Search models..."
          />
          {modelsFetchMessage && (
            <div className="text-xs text-muted-foreground">{modelsFetchMessage}</div>
          )}
          <div className="max-h-40 overflow-auto rounded border p-2 text-sm">
            {filteredModelPool.map((id) => (
              <div key={id} className="flex items-center justify-between py-1">
                <span className="font-mono text-xs">{id}</span>
                <div className="flex gap-1">
                  {(Object.keys(TIER_LABELS) as TierKey[]).map((tier) => (
                    <Button
                      key={tier}
                      size="sm"
                      variant="outline"
                      className="h-6 px-2 text-[10px]"
                      onClick={() => addModelToTier(tier, id)}
                    >
                      + {TIER_LABELS[tier]}
                    </Button>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>

        <div className="grid gap-4 md:grid-cols-2">
          <div className="space-y-1">
            <Label>Embedding Model</Label>
            <Input
              value={configDraft.models.embeddingModel ?? ''}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  models: {
                    ...prev.models,
                    embeddingModel: e.target.value || null,
                  },
                }))
              }
              placeholder="model id, or provider@model to pin one provider"
            />
          </div>
          <div className="space-y-1">
            <Label>Embedding Dimensions</Label>
            <Input
              type="number"
              min={1}
              step={1}
              value={configDraft.models.embeddingDimensions ?? ''}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  models: {
                    ...prev.models,
                    embeddingDimensions:
                      e.target.value.trim() === ''
                        ? null
                        : Number.parseInt(e.target.value, 10),
                  },
                }))
              }
              placeholder="optional; required by some providers"
            />
          </div>
        </div>

        {(Object.keys(TIER_LABELS) as TierKey[]).map((tier) => (
          <div key={tier} className="space-y-1">
            <Label>{TIER_LABELS[tier]}</Label>
            <div className="rounded border divide-y">
              {configDraft.models[tier].map((id, index) => (
                <div key={`${tier}-${id}-${index}`} className="flex items-center justify-between px-3 py-2">
                  <span className="font-mono text-xs">{id}</span>
                  <div className="flex items-center gap-1">
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7"
                      onClick={() => moveModelInTier(tier, index, -1)}
                      disabled={index === 0}
                    >
                      <ArrowUp className="h-3 w-3" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7"
                      onClick={() => moveModelInTier(tier, index, 1)}
                      disabled={index === configDraft.models[tier].length - 1}
                    >
                      <ArrowDown className="h-3 w-3" />
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="h-7 w-7"
                      onClick={() => removeModelFromTier(tier, id)}
                    >
                      <Trash2 className="h-3 w-3" />
                    </Button>
                  </div>
                </div>
              ))}
              {configDraft.models[tier].length === 0 && (
                <div className="px-3 py-2 text-xs text-muted-foreground">No models in this tier</div>
              )}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
