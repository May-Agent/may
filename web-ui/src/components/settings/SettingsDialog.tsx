import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { useStore } from '@/lib/store';
import { Type, Monitor, Moon, Sun, Laptop } from 'lucide-react';
import { Separator } from '@/components/ui/separator';

interface SettingsDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

const FONTS = [
  { name: 'Inter', value: 'Inter', family: 'Inter, sans-serif' },
  { name: 'Roboto', value: 'Roboto', family: 'Roboto, sans-serif' },
  { name: 'Open Sans', value: 'Open Sans', family: '"Open Sans", sans-serif' },
  { name: 'Lato', value: 'Lato', family: 'Lato, sans-serif' },
  { name: 'Montserrat', value: 'Montserrat', family: 'Montserrat, sans-serif' },
  { name: 'Source Han Sans', value: 'Source Han Sans SC', family: '"Source Han Sans SC", "Noto Sans SC", sans-serif' },
  { name: 'Source Han Serif', value: 'Source Han Serif SC', family: '"Source Han Serif SC", "Noto Serif SC", serif' },
];

export function SettingsDialog({ open, onOpenChange }: SettingsDialogProps) {
  const { settings, updateSettings } = useStore();

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-[500px] p-0 overflow-hidden bg-background border-border">
        <DialogHeader className="p-6 pb-4 border-b">
          <DialogTitle className="text-xl">Settings</DialogTitle>
          <DialogDescription>
            Customize your chat experience
          </DialogDescription>
        </DialogHeader>
        
        <div className="p-6 space-y-6">
          {/* Appearance Section */}
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-sm font-medium text-muted-foreground uppercase tracking-wider">
              <Monitor className="h-4 w-4" />
              Appearance
            </div>
            
            <div className="grid gap-4">
              <div className="grid grid-cols-[120px_1fr] items-center gap-4">
                <div className="space-y-1">
                  <Label htmlFor="theme" className="text-base font-medium">Theme</Label>
                  <p className="text-xs text-muted-foreground">App color scheme</p>
                </div>
                <Select
                  value={settings.theme}
                  onValueChange={(value: any) => updateSettings({ theme: value })}
                >
                  <SelectTrigger id="theme" className="w-full h-10">
                    <SelectValue placeholder="Select theme" />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="light">
                      <div className="flex items-center gap-2">
                        <Sun className="h-4 w-4" /> Light
                      </div>
                    </SelectItem>
                    <SelectItem value="dark">
                      <div className="flex items-center gap-2">
                        <Moon className="h-4 w-4" /> Dark
                      </div>
                    </SelectItem>
                    <SelectItem value="system">
                      <div className="flex items-center gap-2">
                        <Laptop className="h-4 w-4" /> System
                      </div>
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
          </div>

          <Separator />

          {/* Typography Section */}
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-sm font-medium text-muted-foreground uppercase tracking-wider">
              <Type className="h-4 w-4" />
              Typography
            </div>
            
            <div className="grid gap-4">
              <div className="grid grid-cols-[120px_1fr] items-center gap-4">
                <div className="space-y-1">
                  <Label htmlFor="font" className="text-base font-medium">Font Family</Label>
                  <p className="text-xs text-muted-foreground">Main text style</p>
                </div>
                <Select
                  value={settings.font}
                  onValueChange={(value) => updateSettings({ font: value })}
                >
                  <SelectTrigger id="font" className="w-full h-10">
                    <SelectValue placeholder="Select font" />
                  </SelectTrigger>
                  <SelectContent>
                    {FONTS.map((font) => (
                      <SelectItem key={font.value} value={font.value} style={{ fontFamily: font.family }}>
                        {font.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

