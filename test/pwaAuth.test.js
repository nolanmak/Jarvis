// #2706 — /queue and /api/push/* sit behind requireAuth, and the service
// worker never serves a cached login page (or any redirect) under /queue.

const test = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");
const vm = require("node:vm");

const tmpDb = path.join(os.tmpdir(), `aa-pwa-auth-${process.pid}.db`);
process.env.AUGMENTAGENT_DB = tmpDb;
process.env.MODE = "local";
process.env.AUGMENTAGENT_API_KEY = "synthetic-pwa-key";

const express = require("express");
const { initDb } = require(path.join(__dirname, "..", "dist", "db.js"));
const apiV1Router = require(path.join(__dirname, "..", "dist", "apiV1.js")).default;

function start() {
  initDb(tmpDb);
  const app = express();
  app.use(express.json());
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));
  app.use(apiV1Router);
  return new Promise((r) => {
    const s = app.listen(0, "127.0.0.1", () => r(s));
  });
}

function request(port, method, p, { headers = {}, body } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: "127.0.0.1", port, path: p, method, headers: { "content-type": "application/json", ...headers } },
      (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () => resolve({ status: res.statusCode, location: res.headers.location, body: b }));
      }
    );
    req.on("error", reject);
    if (body !== undefined) req.write(JSON.stringify(body));
    req.end();
  });
}

test.after(() => {
  for (const ext of ["", "-wal", "-shm"]) {
    try { fs.unlinkSync(tmpDb + ext); } catch {}
  }
});

const SUB = { endpoint: "https://push.example/pwa", keys: { p256dh: "p", auth: "a" } };

test("unauthenticated /queue and /api/push/* get a login redirect or 401", async () => {
  const s = await start();
  const port = s.address().port;
  try {
    const page = await request(port, "GET", "/queue", { headers: { accept: "text/html" } });
    assert.equal(page.status, 302);
    assert.equal(page.location, "/login");
    assert.equal((await request(port, "GET", "/queue")).status, 401);
    assert.equal((await request(port, "GET", "/api/push/vapid")).status, 401);
    assert.equal((await request(port, "POST", "/api/push/subscribe", { body: SUB })).status, 401);
    assert.equal((await request(port, "POST", "/api/push/unsubscribe", { body: { endpoint: SUB.endpoint } })).status, 401);
  } finally {
    s.close();
  }
});

test("authenticated /queue and /api/push/* still work", async () => {
  const s = await start();
  const port = s.address().port;
  const auth = { authorization: "Bearer synthetic-pwa-key" };
  try {
    assert.equal((await request(port, "GET", "/queue", { headers: { ...auth, accept: "text/html" } })).status, 200);
    assert.equal((await request(port, "GET", "/api/push/vapid", { headers: auth })).status, 200);
    assert.equal((await request(port, "POST", "/api/push/subscribe", { headers: auth, body: SUB })).status, 201);
    assert.equal((await request(port, "POST", "/api/push/unsubscribe", { headers: auth, body: { endpoint: SUB.endpoint } })).status, 200);
  } finally {
    s.close();
  }
});

// --- service worker -------------------------------------------------------

function loadServiceWorker({ network }) {
  const listeners = {};
  const stores = new Map();
  const store = (name) => {
    if (!stores.has(name)) stores.set(name, new Map());
    const m = stores.get(name);
    return {
      addAll: async (urls) => urls.forEach((u) => m.set(u, { url: u, status: 200 })),
      put: async (req, res) => m.set(typeof req === "string" ? req : new URL(req.url).pathname, res),
      match: async (req) => m.get(typeof req === "string" ? req : new URL(req.url).pathname),
    };
  };
  const caches = {
    open: async (name) => store(name),
    keys: async () => [...stores.keys()],
    delete: async (name) => stores.delete(name),
    match: async (req) => {
      for (const name of stores.keys()) {
        const hit = await store(name).match(req);
        if (hit) return hit;
      }
      return undefined;
    },
  };
  const self = {
    addEventListener: (type, fn) => (listeners[type] = fn),
    skipWaiting: async () => {},
    clients: { claim: async () => {} },
    registration: {},
  };
  const src = fs.readFileSync(path.join(__dirname, "..", "public", "sw.js"), "utf8");
  vm.runInNewContext(src, { self, caches, fetch: network, URL, Promise, console, Response: { error: () => ({ type: "error" }) } });
  const fetchEvent = async (url, { mode = "navigate", accept = "text/html" } = {}) => {
    let responded;
    listeners.fetch({
      request: { url: `https://jarvis.example${url}`, method: "GET", mode, headers: { get: (h) => (h.toLowerCase() === "accept" ? accept : null) } },
      respondWith: (p) => (responded = p),
    });
    return responded === undefined ? undefined : await responded;
  };
  const lifecycle = async (type) => {
    let wait;
    listeners[type]({ waitUntil: (p) => (wait = p) });
    await wait;
  };
  return { stores, caches, fetchEvent, lifecycle };
}

const ok = (body) => ({ ok: true, status: 200, type: "basic", redirected: false, body, clone() { return this; } });

test("service worker bumps the cache and drops the old one", async () => {
  const sw = loadServiceWorker({ network: async () => ok("x") });
  sw.stores.set("aa-shell-v1", new Map([["/queue", { body: "stale login page" }]]));
  await sw.lifecycle("install");
  await sw.lifecycle("activate");
  assert.ok(!sw.stores.has("aa-shell-v1"), "old cache deleted");
  assert.equal(sw.stores.size, 1);
  for (const [, m] of sw.stores) assert.ok(!m.has("/queue"), "/queue is not precached");
});

test("service worker is network-first for HTML and falls back to cache offline", async () => {
  let online = true;
  let page = "queue v1";
  const sw = loadServiceWorker({
    network: async () => {
      if (!online) throw new Error("offline");
      return ok(page);
    },
  });
  await sw.lifecycle("install");
  await sw.lifecycle("activate");
  assert.equal((await sw.fetchEvent("/queue")).body, "queue v1");
  page = "queue v2";
  assert.equal((await sw.fetchEvent("/queue")).body, "queue v2", "network wins over cache");
  online = false;
  assert.equal((await sw.fetchEvent("/queue")).body, "queue v2", "offline serves last good copy");
});

test("service worker never caches redirects", async () => {
  let response = { ok: false, status: 0, type: "opaqueredirect", redirected: false, body: "login", clone() { return this; } };
  let online = true;
  const sw = loadServiceWorker({
    network: async () => {
      if (!online) throw new Error("offline");
      return response;
    },
  });
  await sw.lifecycle("install");
  await sw.lifecycle("activate");
  assert.equal((await sw.fetchEvent("/queue")).type, "opaqueredirect");
  response = { ...ok("login page"), redirected: true };
  assert.equal((await sw.fetchEvent("/queue")).body, "login page");
  online = false;
  const offline = await sw.fetchEvent("/queue");
  assert.ok(!offline || offline.body !== "login page", "redirected login page was not cached");
  for (const [, m] of sw.stores) assert.ok(!m.has("/queue"));
});
