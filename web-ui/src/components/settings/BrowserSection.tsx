import { Globe } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
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
              checked={configDraft.browser.enabled}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  browser: {
                    ...prev.browser,
                    enabled: e.target.checked,
                  },
                }))
              }
            />
            <span className="text-xs text-muted-foreground">
              May provisions and manages the browser runtime when this is enabled.
              When disabled, the agent cannot use browser automation.
            </span>
          </div>
        </div>

        <div className="space-y-1">
          <Label>Backend</Label>
          <select
            className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            value={configDraft.browser.backend}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                browser: {
                  ...prev.browser,
                  backend: e.target.value as 'managed' | 'remote_cdp',
                },
              }))
            }
          >
            <option value="managed">Managed local browser</option>
            <option value="remote_cdp">Remote CDP browser</option>
          </select>
          <p className="text-xs text-muted-foreground">
            Managed launches a May-owned local browser runtime. Remote CDP connects to a
            May-owned browser endpoint, such as a cloud-hosted browser service.
          </p>
        </div>

        {configDraft.browser.backend === 'managed' ? (
          <div className="space-y-1">
            <Label>CDP Port</Label>
            <Input
              type="number"
              min={1024}
              max={65535}
              value={configDraft.browser.port}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  browser: {
                    ...prev.browser,
                    port: Number(e.target.value || 9222),
                  },
                }))
              }
            />
          </div>
        ) : (
          <div className="space-y-1">
            <Label>Remote CDP Endpoint</Label>
            <Input
              type="url"
              placeholder="wss://browser.example/devtools/browser/..."
              value={configDraft.browser.remoteCdpEndpoint ?? ''}
              onChange={(e) =>
                setDraft((prev) => ({
                  ...prev,
                  browser: {
                    ...prev.browser,
                    remoteCdpEndpoint: e.target.value,
                  },
                }))
              }
            />
            <p className="text-xs text-muted-foreground">
              Use the CDP endpoint for a browser May manages outside this machine.
            </p>
          </div>
        )}

        <div className="space-y-1">
          <Label>Screencast Quality (1-100)</Label>
          <Input
            type="number"
            min={1}
            max={100}
            value={configDraft.browser.screencastQuality}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                browser: {
                  ...prev.browser,
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
            value={configDraft.browser.screencastMaxWidth}
            onChange={(e) =>
              setDraft((prev) => ({
                ...prev,
                browser: {
                  ...prev.browser,
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
