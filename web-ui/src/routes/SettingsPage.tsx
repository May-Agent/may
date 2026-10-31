import { type ComponentType } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  Palette,
  Languages,
  Users,
  Cpu,
  KeyRound,
  Server,
  Sparkles,
  Globe2,
  Bell,
  Brain,
  CalendarClock,
  Shield,
  Activity,
  Cloud,
  Network,
} from 'lucide-react';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { SidebarToggle } from '@/components/shell/SidebarToggle';
import { SIDEBAR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';
import { useTranslation, type MessageKey } from '@/lib/i18n';
import { AppearanceSection } from '@/components/settings/AppearanceSection';
import { LanguageSection } from '@/components/settings/LanguageSection';
import { ProfilesSection } from '@/components/settings/ProfilesSection';
import { ModelsSection } from '@/components/settings/ModelsSection';
import { ProvidersSection } from '@/components/settings/ProvidersSection';
import { McpSection } from '@/components/settings/McpSection';
import { SkillsSection } from '@/components/settings/SkillsSection';
import { BrowserSection } from '@/components/settings/BrowserSection';
import { NotificationsSection } from '@/components/settings/NotificationsSection';
import { MemorySection } from '@/components/settings/MemorySection';
import { TriggersSection } from '@/components/settings/TriggersSection';
import { NetworkSection } from '@/components/settings/NetworkSection';
import { ActivityLogsSection } from '@/components/settings/ActivityLogsSection';
import { RemoteAccessSection } from '@/components/settings/RemoteAccessSection';
import { RemoteServersSection } from '@/components/settings/RemoteServersSection';
import { CommandPrioritySection } from '@/components/settings/CommandPrioritySection';

type SectionId =
  | 'appearance'
  | 'language'
  | 'profiles'
  | 'models'
  | 'providers'
  | 'mcp'
  | 'skills'
  | 'browser'
  | 'notifications'
  | 'memory'
  | 'triggers'
  | 'network'
  | 'logs'
  | 'remote'
  | 'remote-servers'
  | 'execution';

interface Section {
  id: SectionId;
  icon: typeof Palette;
  labelKey: MessageKey;
  Component: ComponentType;
}

interface SectionGroup {
  id: string;
  label: string;
  items: Section[];
}

const GROUPS: SectionGroup[] = [
  {
    id: 'preferences',
    label: 'Preferences',
    items: [
      { id: 'appearance', icon: Palette, labelKey: 'settings.sections.appearance', Component: AppearanceSection },
      { id: 'language', icon: Languages, labelKey: 'settings.sections.language', Component: LanguageSection },
    ],
  },
  {
    id: 'agent',
    label: 'Agent',
    items: [
      { id: 'profiles', icon: Users, labelKey: 'settings.sections.profiles', Component: ProfilesSection },
      { id: 'models', icon: Cpu, labelKey: 'settings.sections.models', Component: ModelsSection },
      { id: 'providers', icon: KeyRound, labelKey: 'settings.sections.providers', Component: ProvidersSection },
      { id: 'mcp', icon: Server, labelKey: 'settings.sections.mcp', Component: McpSection },
      { id: 'skills', icon: Sparkles, labelKey: 'settings.sections.skills', Component: SkillsSection },
      { id: 'memory', icon: Brain, labelKey: 'settings.sections.memory', Component: MemorySection },
      { id: 'triggers', icon: CalendarClock, labelKey: 'settings.sections.triggers', Component: TriggersSection },
    ],
  },
  {
    id: 'runtime',
    label: 'Runtime',
    items: [
      { id: 'execution', icon: Cpu, labelKey: 'settings.sections.execution', Component: CommandPrioritySection },
      { id: 'browser', icon: Globe2, labelKey: 'settings.sections.browser', Component: BrowserSection },
      { id: 'notifications', icon: Bell, labelKey: 'settings.sections.notifications', Component: NotificationsSection },
      { id: 'network', icon: Shield, labelKey: 'settings.sections.network', Component: NetworkSection },
      { id: 'logs', icon: Activity, labelKey: 'settings.sections.logs', Component: ActivityLogsSection },
      { id: 'remote', icon: Cloud, labelKey: 'settings.sections.remote', Component: RemoteAccessSection },
      { id: 'remote-servers', icon: Network, labelKey: 'settings.sections.remoteServers', Component: RemoteServersSection },
    ],
  },
];

const ALL_SECTIONS: Section[] = GROUPS.flatMap((g) => g.items);
const SECTION_IDS: ReadonlySet<SectionId> = new Set(ALL_SECTIONS.map((s) => s.id));
const DEFAULT_SECTION: SectionId = 'appearance';

function parseSection(value: string | null): SectionId {
  if (value && SECTION_IDS.has(value as SectionId)) return value as SectionId;
  return DEFAULT_SECTION;
}

export function SettingsPage() {
  const { t } = useTranslation();
  const [params, setParams] = useSearchParams();
  const active = parseSection(params.get('section'));
  const section = ALL_SECTIONS.find((s) => s.id === active) ?? ALL_SECTIONS[0]!;
  const ActiveComponent = section.Component;

  const selectSection = (id: SectionId) => {
    const next = new URLSearchParams(params);
    next.set('section', id);
    setParams(next, { replace: false });
  };

  // Narrow screens navigate list → detail, like iOS Settings.
  const wide = useMediaQuery(SIDEBAR_COLUMN_QUERY);
  const showList = wide || !params.has('section');
  const showDetail = wide || params.has('section');
  const backToList = () => {
    const next = new URLSearchParams(params);
    next.delete('section');
    setParams(next, { replace: false });
  };

  return (
    <div className="flex h-full overflow-hidden">
      {showList && (
        <nav
          aria-label={t('settings.title')}
          className={cn(
            'shrink-0 overflow-y-auto bg-sidebar px-2 pb-4',
            wide ? 'w-60 border-r border-border' : 'w-full',
          )}
        >
          <div className="flex h-11 items-center gap-1 touch:h-14">
            <SidebarToggle />
          </div>
          <div className="px-3 pb-4">
            <h1 className="text-[15px] font-semibold tracking-tight">{t('settings.title')}</h1>
            <p className="mt-0.5 text-xs text-muted-foreground">{t('settings.subtitle')}</p>
          </div>
          <div className="space-y-4">
            {GROUPS.map((group) => (
              <div key={group.id}>
                <h2 className="px-3 pb-1 text-xs font-semibold text-muted-foreground">
                  {group.label}
                </h2>
                <ul className="space-y-px">
                  {group.items.map((s) => {
                    const Icon = s.icon;
                    const isActive = wide && s.id === active;
                    return (
                      <li key={s.id}>
                        <button
                          type="button"
                          onClick={() => selectSection(s.id)}
                          aria-current={isActive ? 'page' : undefined}
                          className={cn(
                            'flex h-8 w-full items-center gap-2 rounded-md px-3 text-[13px] transition-colors touch:h-11',
                            isActive
                              ? 'bg-accent font-medium text-foreground'
                              : 'text-foreground hover:bg-accent/60',
                          )}
                        >
                          <Icon className="size-4 text-muted-foreground" aria-hidden />
                          <span className="flex-1 text-left">{t(s.labelKey)}</span>
                          {!wide && <ChevronRight className="size-4 text-muted-foreground" aria-hidden />}
                        </button>
                      </li>
                    );
                  })}
                </ul>
              </div>
            ))}
          </div>
        </nav>
      )}
      {showDetail && (
        <div className="min-w-0 flex-1 overflow-y-auto">
          {!wide && (
            <div className="sticky top-0 z-10 flex h-11 items-center border-b border-border bg-background/90 px-2 backdrop-blur touch:h-14">
              <Button variant="ghost" className="gap-1 px-2 text-foreground" onClick={backToList}>
                <ChevronLeft aria-hidden />
                {t('settings.title')}
              </Button>
            </div>
          )}
          <div className="mx-auto max-w-3xl px-4 py-6 sm:px-8 sm:py-8">
            <ActiveComponent />
          </div>
        </div>
      )}
    </div>
  );
}
