import { useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { Button } from '@/components/ui/button';
import { Separator } from '@/components/ui/separator';
import { useStore } from '@/lib/store';
import type { UiFullConfig } from '@/lib/api';
import { SECTIONS, type SettingsSection } from '@/components/settings/constants';
import { AppearanceSection } from '@/components/settings/AppearanceSection';
import { ProvidersSection } from '@/components/settings/ProvidersSection';
import { ModelsSection } from '@/components/settings/ModelsSection';
import { PreferencesSection } from '@/components/settings/PreferencesSection';
import { MemorySection } from '@/components/settings/MemorySection';
import { LogsSection } from '@/components/settings/LogsSection';
import { SshSection } from '@/components/settings/SshSection';
import { McpSection } from '@/components/settings/McpSection';
import { SkillsSection } from '@/components/settings/SkillsSection';
import { TasksSection } from '@/components/settings/TasksSection';
import { BrowserSection } from '@/components/settings/BrowserSection';
import { DangerSection } from '@/components/settings/DangerSection';

export function SettingsPage() {
  const { uiConfig, saveFullConfig } = useStore();

  const [searchParams, setSearchParams] = useSearchParams();
  const activeSectionParam = searchParams.get('section') as SettingsSection | null;
  const activeSection: SettingsSection = SECTIONS.some((s) => s.id === activeSectionParam)
    ? (activeSectionParam as SettingsSection)
    : 'appearance';
  const [configOverride, setConfigOverride] = useState<UiFullConfig | null>(null);
  const [configDirty, setConfigDirty] = useState(false);
  const configDraft = configDirty ? configOverride : uiConfig.fullConfig;

  const setDraft = (updater: (prev: UiFullConfig) => UiFullConfig) => {
    const prev = configDraft;
    if (!prev) return;
    const next = updater(prev);
    setConfigOverride(next);
    setConfigDirty(true);
    void saveFullConfig(next)
      .then(() => setConfigDirty(false))
      .catch((error) => {
        console.error('failed to save config', error);
      });
  };

  const configProps = { configDraft, setDraft };

  return (
    <div className="h-full overflow-hidden bg-background">
      <div className="flex h-full">
        <aside className="w-56 border-r bg-muted/20 p-4 flex flex-col gap-2">
          <div className="text-xs uppercase tracking-wider text-muted-foreground px-2 mb-1">Settings</div>
          {SECTIONS.map((section) => (
            <Button
              key={section.id}
              variant={activeSection === section.id ? 'secondary' : 'ghost'}
              className="justify-start"
              onClick={() => setSearchParams({ section: section.id })}
            >
              {section.label}
            </Button>
          ))}
        </aside>

        <div className="flex-1 overflow-auto">
          <div className="mx-auto max-w-5xl px-8 py-8 space-y-8">
            {activeSection === 'appearance' && <AppearanceSection {...configProps} />}
            {activeSection === 'providers' && <ProvidersSection {...configProps} />}
            {activeSection === 'models' && <ModelsSection {...configProps} />}
            {activeSection === 'preferences' && <PreferencesSection {...configProps} />}
            {activeSection === 'memory' && <MemorySection />}
            {activeSection === 'logs' && <LogsSection />}
            {activeSection === 'ssh' && <SshSection />}
            {activeSection === 'mcp' && <McpSection />}
            {activeSection === 'skills' && <SkillsSection />}
            {activeSection === 'tasks' && <TasksSection />}
            {activeSection === 'browser' && <BrowserSection {...configProps} />}
            {activeSection === 'danger' && <DangerSection />}
            <Separator />
          </div>
        </div>
      </div>
    </div>
  );
}
