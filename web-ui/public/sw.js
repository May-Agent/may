self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const notificationId = event.notification.data?.notificationId;
  event.waitUntil(
    (async () => {
      const clients = await self.clients.matchAll({
        type: 'window',
        includeUncontrolled: true,
      });
      for (const client of clients) {
        client.postMessage({
          type: 'may-notification-click',
          payload: { notificationId },
        });
      }
      const existing = clients.find((c) => 'focus' in c);
      if (existing) {
        return existing.focus();
      }
      if (self.clients.openWindow) {
        const url = notificationId
          ? `/?notification=${encodeURIComponent(notificationId)}`
          : '/';
        return self.clients.openWindow(url);
      }
    })(),
  );
});
