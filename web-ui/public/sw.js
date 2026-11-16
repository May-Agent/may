// Shows the notifications the daemon pushes when a session waits on the
// user, and opens that session when one is clicked.

self.addEventListener('push', (event) => {
  if (!event.data) return;
  const payload = event.data.json();
  event.waitUntil(
    self.registration.showNotification(payload.title, {
      body: payload.body,
      tag: payload.sessionId ?? payload.notificationId,
      renotify: true,
      data: { sessionId: payload.sessionId, profileId: payload.profileId },
    }),
  );
});

function sessionPath({ sessionId, profileId }) {
  if (!sessionId || !profileId) return '/';
  return `/p/${encodeURIComponent(profileId)}/s/${encodeURIComponent(sessionId)}`;
}

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const path = sessionPath(event.notification.data ?? {});
  event.waitUntil(
    (async () => {
      const clients = await self.clients.matchAll({
        type: 'window',
        includeUncontrolled: true,
      });
      const existing = clients.find((c) => 'focus' in c);
      if (existing) {
        const navigated = await existing.navigate(path);
        return (navigated ?? existing).focus();
      }
      return self.clients.openWindow(path);
    })(),
  );
});
