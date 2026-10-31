import { useParams } from 'react-router-dom';
import { ChatView } from '@/components/chat/ChatView';
import { InspectorPane } from '@/components/inspector/InspectorPane';
import { Sheet, SheetContent, SheetTitle } from '@/components/ui/sheet';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useUiStore } from '@/lib/stores/ui';
import { INSPECTOR_COLUMN_QUERY, useMediaQuery } from '@/lib/useMediaQuery';
import { useTranslation } from '@/lib/i18n';

/** Chat in the content area, the inspector beside it (or in a sheet when narrow). */
export function SessionPage() {
  const { sessionId } = useParams();
  const active = useSessionsStore((s) => s.active);
  const inspectorOpen = useUiStore((s) => s.inspectorOpen);
  const inspectorSheetOpen = useUiStore((s) => s.inspectorSheetOpen);
  const setInspectorSheetOpen = useUiStore((s) => s.setInspectorSheetOpen);
  const inspectorColumn = useMediaQuery(INSPECTOR_COLUMN_QUERY);
  const { t } = useTranslation();
  if (!sessionId) return null;
  const session = active && active.id === sessionId ? active : null;

  return (
    <div className="flex h-full min-w-0">
      <div className="min-w-0 flex-1">
        <ChatView sessionId={sessionId} />
      </div>
      {session && inspectorColumn && inspectorOpen && (
        <aside aria-label={t('inspector.title')} className="w-80 shrink-0 border-l border-border">
          <InspectorPane session={session} />
        </aside>
      )}
      {session && !inspectorColumn && (
        <Sheet open={inspectorSheetOpen} onOpenChange={setInspectorSheetOpen}>
          <SheetContent side="right" className="w-[min(22rem,92vw)] p-0" hideCloseButton aria-describedby={undefined}>
            <SheetTitle className="sr-only">{t('inspector.title')}</SheetTitle>
            <InspectorPane session={session} inSheet />
          </SheetContent>
        </Sheet>
      )}
    </div>
  );
}
