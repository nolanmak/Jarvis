// #45 — AugmentAgent PWA service worker.
//
// Two jobs: (1) a minimal offline app-shell cache so the /queue route opens
// without network, (2) Web Push display + click-through. Push payloads are
// JSON: { actionId, title, body }. Clicking a notification focuses (or opens)
// the /queue route deep-linked to that action.

const CACHE = "aa-shell-v1";
const SHELL = ["/queue", "/manifest.json", "/styles.css"];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))
      )
      .then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  // Network-first for API/data, cache-first for the shell.
  if (SHELL.includes(url.pathname)) {
    event.respondWith(
      caches.match(event.request).then((r) => r || fetch(event.request))
    );
  }
});

self.addEventListener("push", (event) => {
  let data = {};
  try {
    data = event.data ? event.data.json() : {};
  } catch (e) {
    data = { title: "AugmentAgent", body: "New approval" };
  }
  event.waitUntil(
    self.registration.showNotification(data.title || "Approval pending", {
      body: data.body || "Tap to review the drafted reply.",
      tag: data.actionId || "aa-approval",
      data: { actionId: data.actionId || "" },
      badge: "/icon-192.png",
      icon: "/icon-192.png",
    })
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const id = event.notification.data && event.notification.data.actionId;
  const target = id ? `/queue?action=${encodeURIComponent(id)}` : "/queue";
  event.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((cs) => {
        for (const c of cs) {
          if (c.url.includes("/queue") && "focus" in c) return c.focus();
        }
        return self.clients.openWindow(target);
      })
  );
});
