import { useState } from 'react';
import { Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import { stopTakeover } from '@/lib/api/browser';
import { useBrowserViewStore } from '@/lib/stores/browserView';
import type { ApiSession } from '@/lib/types';

/** While you drive the agent's browser: one quiet line to hand it back. */
export function BrowserTakeoverBanner({ session }: { session: ApiSession }) {
  const { t } = useTranslation();
  const openViewer = useBrowserViewStore((s) => s.openViewer);
  const [busy, setBusy] = useState(false);
  const takeover = session.runtime.interactions?.browser;
  if (!takeover?.takeoverActive) return null;

  const handBack = async () => {
    setBusy(true);
    try {
      await stopTakeover(session.id);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center gap-3 border-b border-border px-4 py-1.5 text-[13px] sm:px-6">
      <span className="min-w-0 flex-1 truncate text-muted-foreground">{t('browser.youHaveControl')}</span>
      <Button size="sm" variant="ghost" onClick={openViewer}>
        {t('browser.open')}
      </Button>
      <Button size="sm" variant="outline" disabled={busy} onClick={() => void handBack()}>
        {busy && <Loader2 className="animate-spin" aria-hidden />}
        {t('browser.handBack')}
      </Button>
    </div>
  );
}
