import { Globe2, LogOut } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTranslation } from '@/lib/i18n';
import { stopTakeover } from '@/lib/api/browser';
import type { ApiSession } from '@/lib/types';
import { useState } from 'react';

export function BrowserTakeoverBanner({ session }: { session: ApiSession }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const takeover = session.runtime.interactions?.browser;
  if (!takeover?.takeoverActive) return null;

  const handleRelease = async () => {
    setBusy(true);
    try {
      await stopTakeover(session.id);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex items-center justify-between gap-3 border-b border-amber-500/40 bg-amber-500/10 px-6 py-2 text-xs">
      <div className="flex items-center gap-2">
        <Globe2 className="size-4 text-amber-600 dark:text-amber-400" />
        <span className="font-medium">{t('browserTakeover.active')}</span>
      </div>
      <Button size="sm" variant="outline" disabled={busy} onClick={handleRelease}>
        <LogOut className="size-3.5" />
        {t('browserTakeover.release')}
      </Button>
    </div>
  );
}
