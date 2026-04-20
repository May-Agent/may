import { Moon, Sun, Laptop } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useUiStore, type Theme } from '@/lib/stores/ui';

const THEME_OPTIONS: Array<{ value: Theme; label: string; icon: typeof Sun }> = [
  { value: 'light', label: 'Light', icon: Sun },
  { value: 'dark', label: 'Dark', icon: Moon },
  { value: 'system', label: 'System', icon: Laptop },
];

export function SettingsPage() {
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);

  return (
    <div className="mx-auto max-w-2xl px-8 py-10">
      <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
      <p className="mt-1 text-sm text-muted-foreground">
        Local preferences. Backend settings live in <code>~/.may/config.toml</code>.
      </p>

      <section className="mt-8 space-y-3">
        <h2 className="text-sm font-medium">Theme</h2>
        <div className="flex gap-2">
          {THEME_OPTIONS.map(({ value, label, icon: Icon }) => (
            <Button
              key={value}
              variant={theme === value ? 'default' : 'outline'}
              onClick={() => setTheme(value)}
            >
              <Icon className="size-4" />
              {label}
            </Button>
          ))}
        </div>
      </section>
    </div>
  );
}
