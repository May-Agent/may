import { useState } from 'react';
import { Folder, Globe2, PanelRight } from 'lucide-react';
import type { ApiSession } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { RenameDialog } from '@/components/ui/rename-dialog';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import { useRuntimeStore } from '@/lib/stores/runtime';
import { useUiStore } from '@/lib/stores/ui';
import { RuntimePhaseBadge } from '@/components/shell/RuntimePhaseDot';
import { useTranslation } from '@/lib/i18n';
import { shortcutAria, shortcutLabel } from '@/lib/shortcuts';
import { INSPECTOR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';
import { SidebarToggle } from '@/components/shell/SidebarToggle';
import { ContextRing } from './ContextRing';
import { ArtifactsDrawer } from './ArtifactsDrawer';

/**
 * The window toolbar: sidebar toggle, the chat's title (click to rename),
 * its state, and the session tools, ending with the inspector toggle.
 */
export function SessionHeader({ session }: { session: ApiSession }) {
  const rename = useSessionsStore((s) => s.rename);
  const [renaming, setRenaming] = useState(false);
  const [artifactsOpen, setArtifactsOpen] = useState(false);
  const browserViewOpen = useBrowserViewStore((s) => s.open);
  const toggleBrowserView = useBrowserViewStore((s) => s.toggle);
  const browserEnabled = useRuntimeStore((s) => s.browser?.enabled ?? false);
  const inspectorOpen = useUiStore((s) => s.inspectorOpen);
  const toggleInspector = useUiStore((s) => s.toggleInspector);
  const inspectorSheetOpen = useUiStore((s) => s.inspectorSheetOpen);
  const setInspectorSheetOpen = useUiStore((s) => s.setInspectorSheetOpen);
  const inspectorColumn = useMediaQuery(INSPECTOR_COLUMN_QUERY);
  const { t } = useTranslation();

  const title = session.title || t('common.untitled');
  const inspectorShown = inspectorColumn ? inspectorOpen : inspectorSheetOpen;
  const inspectorLabel = inspectorShown ? t('shell.hideInspector') : t('shell.showInspector');

  return (
    <header className="flex h-12 shrink-0 items-center gap-1 border-b border-border bg-background/80 px-2 backdrop-blur supports-[backdrop-filter]:bg-background/70 touch:h-14 sm:px-3">
      <SidebarToggle />
      <h1 className="min-w-0 flex-1">
        <button
          type="button"
          onClick={() => setRenaming(true)}
          title={t('shell.renameTitle')}
          className="max-w-full truncate rounded-md px-2 py-1 text-left text-[15px] font-semibold hover:bg-accent touch:min-h-11"
        >
          {title}
        </button>
      </h1>
      <div className="hidden sm:block">
        <RuntimePhaseBadge state={session.runtime?.state} />
      </div>
      <ContextRing sessionId={session.id} />
      {browserEnabled && (
        <Button
          size="icon"
          variant={browserViewOpen ? 'secondary' : 'ghost'}
          onClick={toggleBrowserView}
          aria-pressed={browserViewOpen}
          aria-label={browserViewOpen ? t('browserLive.close') : t('browserLive.open')}
          title={browserViewOpen ? t('browserLive.close') : t('browserLive.open')}
        >
          <Globe2 />
        </Button>
      )}
      <Button
        size="icon"
        variant="ghost"
        onClick={() => setArtifactsOpen(true)}
        aria-label={t('chat.artifacts')}
        title={t('chat.artifacts')}
      >
        <Folder />
      </Button>
      <Button
        size="icon"
        variant={inspectorShown ? 'secondary' : 'ghost'}
        onClick={() => (inspectorColumn ? toggleInspector() : setInspectorSheetOpen(!inspectorSheetOpen))}
        aria-pressed={inspectorShown}
        aria-label={inspectorLabel}
        aria-keyshortcuts={shortcutAria('toggleInspector')}
        title={`${inspectorLabel} (${shortcutLabel('toggleInspector')})`}
      >
        <PanelRight />
      </Button>
      <ArtifactsDrawer sessionId={session.id} open={artifactsOpen} onOpenChange={setArtifactsOpen} />
      <RenameDialog
        open={renaming}
        onOpenChange={setRenaming}
        title={t('shell.renameTitle')}
        label={t('shell.renameLabel')}
        initialValue={session.title}
        onSubmit={(next) => rename(session.id, next)}
      />
    </header>
  );
}
