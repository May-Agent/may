import { SlidersHorizontal } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import type { UiFullConfig } from '@/lib/api';
import type { ConfigSectionProps } from './constants';

export function PreferencesSection({ configDraft, setDraft }: ConfigSectionProps) {
  if (!configDraft) {
    return <div className="text-sm text-muted-foreground">Loading preferences...</div>;
  }

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <SlidersHorizontal className="h-4 w-4" /> Preferences
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <div className="space-y-1">
          <Label>Load Balance Strategy</Label>
          <Select
            value={configDraft.preferences.loadBalanceStrategy}
            onValueChange={(v) =>
              setDraft((prev) => ({
                ...prev,
                preferences: {
                  ...prev.preferences,
                  loadBalanceStrategy: v as UiFullConfig['preferences']['loadBalanceStrategy'],
                },
              }))
            }
          >
            <SelectTrigger><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="round_robin">round_robin</SelectItem>
              <SelectItem value="failover_only">failover_only</SelectItem>
              <SelectItem value="random">random</SelectItem>
            </SelectContent>
          </Select>
        </div>

        <div className="space-y-1">
          <Label>Last model</Label>
          <Input
            value={configDraft.lastModel ?? ''}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                lastModel: e.target.value || null,
              }))
            }
            placeholder="e.g. claude-sonnet-4"
          />
        </div>

        <div className="space-y-1">
          <Label>Max iterations</Label>
          <Input
            type="number"
            min={0}
            value={configDraft.maxIterations}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                maxIterations: Math.max(0, Number(e.target.value || 0)),
              }))
            }
          />
          <p className="text-xs text-muted-foreground">0 means unlimited.</p>
        </div>

        <div className="space-y-1">
          <Label className="text-sm">Warn on model change</Label>
          <div className="pt-2">
            <input
              type="checkbox"
              checked={configDraft.preferences.warnOnModelChange}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  preferences: {
                    ...prev.preferences,
                    warnOnModelChange: e.target.checked,
                  },
                }))
              }
            />
          </div>
        </div>

        <div className="space-y-1">
          <Label className="text-sm">Show token usage in chat</Label>
          <div className="pt-2">
            <input
              type="checkbox"
              checked={configDraft.preferences.showTokenUsage}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  preferences: {
                    ...prev.preferences,
                    showTokenUsage: e.target.checked,
                  },
                }))
              }
            />
          </div>
        </div>

        <div className="space-y-1 md:col-span-2">
          <Label className="text-sm">Strict model matching</Label>
          <div className="pt-2 flex items-center gap-3">
            <input
              type="checkbox"
              checked={configDraft.preferences.strictModelMatching}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  preferences: {
                    ...prev.preferences,
                    strictModelMatching: e.target.checked,
                  },
                }))
              }
            />
            <span className="text-xs text-muted-foreground">
              Off (default): treat -preview and date-suffixed variants as the same base model for fallback matching.
            </span>
          </div>
        </div>
      </div>
    </section>
  );
}
