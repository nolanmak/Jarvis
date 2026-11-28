// #45 — AugmentAgent PWA service worker.
//
// Two jobs: (1) keep the /queue page usable offline, (2) Web Push display +
// click-through. Push payloads are JSON: { actionId, title, body }. Clicking a
// notification focuses (or opens) the /queue route deep-linked to that action.
//
// #2706: /queue is behind auth, so HTML is network-first (the server decides
// every time) and only a plain 200 is cached. Redirects (the login page, an
// edge sign-in) are never cached, so a login page can't be served as /queue.

const CACHE = "aa-shell-v2";
// Static assets served before auth; safe to precache.
const SHELL = ["/manifest.json", "/styles.css"];
// HTML pages kept for offline use, keyed by path (the query is ignored).
const OFFLINE_PAGES = ["/queue"];

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

function isCacheable(res) {
  return res && res.ok && res.status === 200 && res.type === "basic" && !res.redirected;
}

function isHtml(request) {
  return request.mode === "navigate" || (request.headers.get("accept") || "").includes("text/html");
}

function networkFirst(request, key) {
  return fetch(request)
    .then((res) => {
      if (isCacheable(res)) {
        const copy = res.clone();
        caches.open(CACHE).then((c) => c.put(key, copy));
      }
      return res;
    })
    .catch(() =>
      caches.open(CACHE).then((c) => c.match(key)).then((r) => r || Response.error())
    );
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (isHtml(request)) {
    if (OFFLINE_PAGES.includes(url.pathname)) {
      event.respondWith(networkFirst(request, url.pathname));
    }
    return;
  }
  if (SHELL.includes(url.pathname)) {
    event.respondWith(caches.match(request).then((r) => r || fetch(request)));
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
