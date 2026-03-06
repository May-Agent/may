import { Monitor } from 'lucide-react';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { useStore } from '@/lib/store';
import { FONTS } from '@/lib/constants';
import type { ConfigSectionProps } from './constants';

export function AppearanceSection({ setDraft }: ConfigSectionProps) {
  const { settings, updateSettings } = useStore();

  return (
    <section className="space-y-4">
      <div className="flex items-center gap-2 text-sm font-medium uppercase text-muted-foreground tracking-wide">
        <Monitor className="h-4 w-4" /> Appearance
      </div>
      <div className="grid gap-4 md:grid-cols-2">
        <div className="space-y-2">
          <Label>Theme</Label>
          <Select
            value={settings.theme}
            onValueChange={(v) => {
              const theme = v as 'light' | 'dark' | 'system';
              updateSettings({ theme });
              setDraft((prev) => ({
                ...prev,
                appearance: { ...prev.appearance, theme },
              }));
            }}
          >
            <SelectTrigger><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="light">Light</SelectItem>
              <SelectItem value="dark">Dark</SelectItem>
              <SelectItem value="system">System</SelectItem>
            </SelectContent>
          </Select>
        </div>

        <div className="space-y-2">
          <Label>Font</Label>
          <Select
            value={settings.font}
            onValueChange={(v) => {
              updateSettings({ font: v });
              setDraft((prev) => ({
                ...prev,
                appearance: { ...prev.appearance, font: v },
              }));
            }}
          >
            <SelectTrigger><SelectValue /></SelectTrigger>
            <SelectContent>
              {FONTS.map((f) => (
                <SelectItem key={f.value} value={f.value} style={{ fontFamily: f.family }}>
                  {f.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>
    </section>
  );
}
