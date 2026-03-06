import { Globe } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import type { ConfigSectionProps } from './constants';

export function BrowserSection({ configDraft, setDraft }: ConfigSectionProps) {
  if (!configDraft) {
    return <div className="text-sm text-muted-foreground">Loading config...</div>;
  }

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <Globe className="h-4 w-4" /> Browser Automation
      </div>

      <div className="grid gap-4 md:grid-cols-2">
        <div className="space-y-1 md:col-span-2">
          <Label className="text-sm">Enable browser automation</Label>
          <div className="pt-2 flex items-center gap-3">
            <input
              type="checkbox"
              checked={configDraft.playwright.enabled}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  playwright: {
                    ...prev.playwright,
                    enabled: e.target.checked,
                  },
                }))
              }
            />
            <span className="text-xs text-muted-foreground">
              Launch a browser with CDP for live view and agent automation.
              Requires restart to take effect.
            </span>
          </div>
        </div>

        <div className="space-y-1">
          <Label>Browser</Label>
          <Select
            value={configDraft.playwright.browser}
            onValueChange={(v) =>
              setDraft((prev) => ({
                ...prev,
                playwright: {
                  ...prev.playwright,
                  browser: v as 'local_chrome' | 'chromium',
                },
              }))
            }
          >
            <SelectTrigger><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="local_chrome">Local Chrome</SelectItem>
              <SelectItem value="chromium">Chromium (Playwright)</SelectItem>
            </SelectContent>
          </Select>
        </div>

        <div className="space-y-1">
          <Label>CDP Port</Label>
          <Input
            type="number"
            min={1024}
            max={65535}
            value={configDraft.playwright.port}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                playwright: {
                  ...prev.playwright,
                  port: Number(e.target.value || 9222),
                },
              }))
            }
          />
        </div>

        <div className="space-y-1">
          <Label>Screencast Quality (1-100)</Label>
          <Input
            type="number"
            min={1}
            max={100}
            value={configDraft.playwright.screencastQuality}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                playwright: {
                  ...prev.playwright,
                  screencastQuality: Number(e.target.value || 60),
                },
              }))
            }
          />
          <p className="text-xs text-muted-foreground">
            Lower quality reduces bandwidth for remote connections.
          </p>
        </div>

        <div className="space-y-1">
          <Label>Max Width (px)</Label>
          <Input
            type="number"
            min={320}
            max={3840}
            step={160}
            value={configDraft.playwright.screencastMaxWidth}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                playwright: {
                  ...prev.playwright,
                  screencastMaxWidth: Number(e.target.value || 1280),
                },
              }))
            }
          />
          <p className="text-xs text-muted-foreground">
            Lower resolution reduces bandwidth. Default: 1280.
          </p>
        </div>
      </div>
    </section>
  );
}
