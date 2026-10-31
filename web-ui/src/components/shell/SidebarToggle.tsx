import { PanelLeft } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useUiStore } from '@/lib/stores/ui';
import { useTranslation } from '@/lib/i18n';
import { shortcutAria, shortcutLabel } from '@/lib/shortcuts';
import { SIDEBAR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';

/** "Show sidebar", rendered only while the sidebar is hidden. */
export function SidebarToggle() {
  const sidebarCollapsed = useUiStore((s) => s.sidebarCollapsed);
  const toggleSidebar = useUiStore((s) => s.toggleSidebar);
  const setSidebarSheetOpen = useUiStore((s) => s.setSidebarSheetOpen);
  const sidebarColumn = useMediaQuery(SIDEBAR_COLUMN_QUERY);
  const { t } = useTranslation();
  if (sidebarColumn && !sidebarCollapsed) return null;
  return (
    <Button
      size="icon"
      variant="ghost"
      onClick={() => (sidebarColumn ? toggleSidebar() : setSidebarSheetOpen(true))}
      aria-label={t('shell.showSidebar')}
      aria-keyshortcuts={shortcutAria('toggleSidebar')}
      title={`${t('shell.showSidebar')} (${shortcutLabel('toggleSidebar')})`}
    >
      <PanelLeft />
    </Button>
  );
}
