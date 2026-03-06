self.addEventListener('push', (event) => {
  if (!event.data) {
    console.error('May Push: missing payload data')
    return
  }

  let payload
  try {
    payload = event.data.json()
  } catch (error) {
    console.error('May Push: invalid JSON payload', error)
    return
  }

  if (typeof payload !== 'object' || payload === null) {
    console.error('May Push: payload must be an object')
    return
  }

  if (typeof payload.title !== 'string' || payload.title.trim().length === 0) {
    console.error('May Push: payload.title must be a non-empty string')
    return
  }

  if (typeof payload.body !== 'string' || payload.body.trim().length === 0) {
    console.error('May Push: payload.body must be a non-empty string')
    return
  }

  event.waitUntil(
    self.registration.showNotification(payload.title.trim(), {
      body: payload.body,
      data: payload,
      tag: 'may-notification',
      renotify: true,
      requireInteraction: false,
    }),
  )
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const payload = event.notification.data || {}

  event.waitUntil((async () => {
    const clientsList = await clients.matchAll({
      type: 'window',
      includeUncontrolled: true,
    })

    if (clientsList.length > 0) {
      const client = clientsList[0]
      await client.focus()
      client.postMessage({
        type: 'may-notification-click',
        payload,
      })
      return
    }

    const opened = await clients.openWindow('/chat/new')
    if (opened) {
      opened.postMessage({
        type: 'may-notification-click',
        payload,
      })
    }
  })())
})
