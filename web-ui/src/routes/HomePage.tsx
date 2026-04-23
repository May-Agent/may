import { useNavigate } from 'react-router-dom';
import { Button } from '@/components/ui/button';
import { useSessionsStore } from '@/lib/stores/sessions';
import { useTranslation } from '@/lib/i18n';
import { useActiveProfile, profileSessionPath } from '@/lib/profile/active';

export function HomePage() {
  const createNew = useSessionsStore((s) => s.createNew);
  const navigate = useNavigate();
  const { t } = useTranslation();
  const { profileId } = useActiveProfile();

  const start = async () => {
    if (!profileId) return;
    const s = await createNew(profileId);
    navigate(profileSessionPath(profileId, s.id));
  };

  return (
    <div className="flex h-full items-center justify-center px-6">
      <div className="max-w-md space-y-4 text-center">
        <div className="text-2xl font-semibold tracking-tight">{t('home.welcome')}</div>
        <p className="text-sm text-muted-foreground">{t('home.description')}</p>
        <Button onClick={start}>{t('home.newChat')}</Button>
      </div>
    </div>
  );
}
