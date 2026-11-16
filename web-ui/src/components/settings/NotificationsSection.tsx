import { useEffect, useState } from 'react';
import { Bell } from 'lucide-react';
import { SectionHeader } from './SectionHeader';
import { useTranslation } from '@/lib/i18n';
import { getMailbox, type NotificationMailboxEntry } from '@/lib/api/notifications';
import { useFormatRelativeTime } from '@/lib/utils';

export function NotificationsSection() {
  const { t } = useTranslation();
  const formatRelative = useFormatRelativeTime();
  const [items, setItems] = useState<NotificationMailboxEntry[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    void getMailbox()
      .then((mailbox) => {
        if (!cancelled) setItems(mailbox.items);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <section>
      <SectionHeader
        title={t('settings.notifications.title')}
        description={t('settings.notifications.description')}
      />

      {loading ? (
        <div className="py-6 text-center text-sm text-muted-foreground">{t('common.loading')}</div>
      ) : items.length === 0 ? (
        <div className="py-6 text-center text-sm text-muted-foreground">
          {t('settings.notifications.empty')}
        </div>
      ) : (
        <ul className="space-y-1">
          {items.map((n) => (
            <li
              key={n.id}
              className="flex items-start gap-2 rounded-md border border-border px-3 py-2 text-sm"
            >
              <Bell className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                {n.title && <div className="font-medium">{n.title}</div>}
                {n.body && <div className="text-xs text-muted-foreground">{n.body}</div>}
                <div className="text-[11px] text-muted-foreground">
                  {formatRelative(n.createdAt)} · {n.status}
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
