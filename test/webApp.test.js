// #2717 — Jarvis Dashboard app shell, threads sidebar and fragment routes
// (spec §5.3 fragment routes, §5.4). Render tests for `/` and `/web/threads`.

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const express = require("express");
const Database = require("better-sqlite3");
const { initDb } = require("../dist/db");
const { ReadDb } = require("../dist/web/readDb");
const { createFragmentRoutes, shortTime } = require("../dist/web/fragments");

const tmpRoot = os.platform() === "darwin" ? "/tmp" : os.tmpdir();
const dir = fs.mkdtempSync(path.join(tmpRoot, "wa-"));
const dbPath = path.join(dir, "data.db");
initDb(dbPath);
test.after(() => fs.rmSync(dir, { recursive: true, force: true }));

{
  const w = new Database(dbPath);
  w.exec(`
    CREATE TABLE web_threads(id TEXT PRIMARY KEY, title TEXT NOT NULL,
      title_set INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL, changed_at INTEGER NOT NULL,
      pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0);`);
  const add = w.prepare(
    "INSERT INTO web_threads(id,title,created_at,updated_at,changed_at,pinned,archived) VALUES (?,?,?,?,?,?,?)",
  );
  const now = Date.now();
  add.run("t-old", "Quarterly planning notes", now - 5e8, now - 5e8, now - 5e8, 0, 0);
  add.run("t-new", "Draft reply to <b>Sam</b>", now - 6e4, now - 6e4, now - 6e4, 0, 0);
  add.run("t-pin", "Travel checklist", now - 9e8, now - 9e8, now - 9e8, 1, 0);
  add.run("t-arc", "Old invoice thread", now - 1e9, now - 1e9, now - 1e9, 0, 1);
  w.close();
}

async function fakeDaemon(t, reply) {
  const sockDir = fs.mkdtempSync(path.join(tmpRoot, "wd-"));
  const socketPath = path.join(sockDir, "web.sock");
  const requests = [];
  const server = net.createServer((conn) => {
    let buf = "";
    conn.on("data", (chunk) => {
      buf += chunk;
      const nl = buf.indexOf("\n");
      if (nl < 0) return;
      const req = JSON.parse(buf.slice(0, nl));
      requests.push(req);
      conn.end(JSON.stringify(reply(req)) + "\n");
    });
  });
  await new Promise((r) => server.listen(socketPath, r));
  fs.chmodSync(socketPath, 0o600);
  const prev = process.env.AUGMENTAGENT_WEB_SOCKET;
  process.env.AUGMENTAGENT_WEB_SOCKET = socketPath;
  t.after(() => {
    if (prev === undefined) delete process.env.AUGMENTAGENT_WEB_SOCKET;
    else process.env.AUGMENTAGENT_WEB_SOCKET = prev;
    server.close();
    fs.rmSync(sockDir, { recursive: true, force: true });
  });
  return requests;
}

async function startApp(t) {
  const readDb = new ReadDb(dbPath);
  const app = express();
  app.use(express.urlencoded({ extended: true }));
  app.set("view engine", "ejs");
  app.set("views", path.join(__dirname, "..", "views"));
  app.use(createFragmentRoutes({ readDb: () => readDb }));
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  t.after(() => {
    server.close();
    readDb.close();
  });
  const base = `http://127.0.0.1:${server.address().port}`;
  return async (method, url, form) => {
    const res = await fetch(base + url, {
      method,
      headers: form ? { "content-type": "application/x-www-form-urlencoded" } : {},
      body: form ? new URLSearchParams(form).toString() : undefined,
    });
    return { status: res.status, headers: res.headers, html: await res.text() };
  };
}

test("GET / renders the app shell with mobile viewport, bottom bar and legacy More menu", async (t) => {
  const call = await startApp(t);
  const res = await call("GET", "/");
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type"), /text\/html/);
  const html = res.html;
  assert.ok(
    html.includes('<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">'),
    "viewport meta",
  );
  assert.match(html, /<link rel="manifest" href="\/manifest\.json" crossorigin="use-credentials"/);
  assert.match(html, /<script src="\/web\.js" defer><\/script>/);
  assert.match(html, /hx-ext="sse" sse-connect="\/api\/web\/events"/);
  // Bottom bar: Threads · Chat · Approvals.
  const bar = html.match(/<nav class="j-tabbar"[\s\S]*?<\/nav>/);
  assert.ok(bar, "bottom bar markup");
  for (const pane of ["threads", "chat", "approvals"]) {
    assert.match(bar[0], new RegExp(`data-pane-target="${pane}"`));
  }
  // No thread selected: the threads pane is the phone default.
  assert.match(html, /<body[^>]*data-pane="threads"/);
  // Legacy pages live under "More".
  const more = html.match(/<details class="j-menu[^"]*">\s*<summary[^>]*aria-label="More pages"[\s\S]*?<\/details>/);
  assert.ok(more && />More</.test(more[0]), "More menu");
  for (const href of ["/dashboard", "/history", "/settings", "/queue", "/audit"]) {
    assert.ok(more[0].includes(`href="${href}"`), href);
  }
  // The sidebar is server-rendered into the first paint.
  assert.match(html, /id="threads"/);
  assert.match(html, /href="\/\?thread=t-pin"/);
});

test("GET /?thread=<id> opens that thread on the chat pane and escapes its title", async (t) => {
  const call = await startApp(t);
  const html = (await call("GET", "/?thread=t-new")).html;
  assert.match(html, /<body[^>]*data-pane="chat"/);
  assert.match(html, /<body[^>]*data-thread="t-new"/);
  assert.ok(html.includes("Draft reply to &lt;b&gt;Sam&lt;/b&gt;"));
  assert.ok(!html.includes("<b>Sam</b>"));
  assert.match(html, /<a[^>]*href="\/\?thread=t-new"[^>]*aria-current="page"/);
  const missing = (await call("GET", "/?thread=nope")).html;
  assert.match(missing, /Thread not found/);
  assert.match(missing, /<body[^>]*data-thread=""/);
});

test("GET /web/threads renders pinned then recent, and the archived toggle", async (t) => {
  const call = await startApp(t);
  const res = await call("GET", "/web/threads?active=t-old");
  assert.equal(res.status, 200);
  const html = res.html;
  assert.match(html, /^<div id="threads"[^>]*hx-get="\/web\/threads\?archived=0&amp;active=t-old"/);
  assert.match(html, /hx-trigger="sse:threads_changed, jarvis:refresh"/);
  const order = ["Pinned", "thread=t-pin", "Recent", "thread=t-new", "thread=t-old"].map((s) => html.indexOf(s));
  assert.ok(order.every((i, k) => i >= 0 && (k === 0 || i > order[k - 1])), `order ${order}`);
  assert.ok(!html.includes("t-arc"));
  assert.ok(html.includes("Draft reply to &lt;b&gt;Sam&lt;/b&gt;"));
  assert.match(html, /<a[^>]*href="\/\?thread=t-old"[^>]*aria-current="page"/);
  assert.match(html, /hx-get="\/web\/threads\?archived=1&amp;active=t-old"/);

  const archived = (await call("GET", "/web/threads?archived=1")).html;
  assert.match(archived, /Archived/);
  assert.ok(archived.includes('href="/?thread=t-arc"'));
  assert.ok(!archived.includes("t-new"));
});

test("POST /web/threads creates a thread, returns the sidebar and pushes its URL", async (t) => {
  const thread = { id: "t-made", title: "New thread", created_at: 1, updated_at: Date.now(), changed_at: 1, pinned: false, archived: false };
  const requests = await fakeDaemon(t, () => ({ ok: true, thread }));
  const call = await startApp(t);
  const res = await call("POST", "/web/threads");
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("hx-push-url"), "/?thread=t-made");
  assert.match(res.html, /^<div id="threads"/);
  assert.equal(requests[0].op, "thread.create");
});

test("POST /web/threads/:id/set coerces form booleans and swaps the thread header out of band", async (t) => {
  const thread = { id: "t-old", title: "Quarterly planning notes", created_at: 1, updated_at: 1, changed_at: 1, pinned: true, archived: false };
  const requests = await fakeDaemon(t, () => ({ ok: true, thread }));
  const call = await startApp(t);
  const res = await call("POST", "/web/threads/t-old/set", { pinned: "true" });
  assert.equal(res.status, 200);
  assert.deepEqual(requests[0], { v: 1, op: "thread.set", thread_id: "t-old", pinned: true });
  assert.match(res.html, /id="thread-header"[^>]*hx-swap-oob="true"/);
  assert.match(res.html, /Unpin/);

  const renamed = await call("POST", "/web/threads/t-old/rename", { title: "Q3 plan" });
  assert.equal(renamed.status, 200);
  assert.equal(requests[1].op, "thread.rename");
  assert.equal(requests[1].title, "Q3 plan");
});

test("fragment errors render inline into the toast with the mapped HTTP status", async (t) => {
  await fakeDaemon(t, () => ({ ok: false, code: "thread_not_found", error: "no such thread" }));
  const call = await startApp(t);
  const res = await call("POST", "/web/threads/zzz/set", { archived: "true" });
  assert.equal(res.status, 404);
  assert.equal(res.headers.get("hx-retarget"), "#j-toast");
  assert.match(res.html, /role="alert"/);
  assert.match(res.html, /Thread not found/);

  delete process.env.AUGMENTAGENT_WEB_SOCKET;
  const off = await call("POST", "/web/threads");
  assert.equal(off.status, 503);
  assert.match(off.html, /Jarvis web chat is not enabled/);
});

test("GET /web/palette ranks actions and threads and links threads to /?thread=<id>", async (t) => {
  const call = await startApp(t);
  const html = (await call("GET", "/web/palette?q=tra")).html;
  assert.ok(html.indexOf("Travel checklist") >= 0);
  assert.match(html, /href="\/\?thread=t-pin"/);
  const all = (await call("GET", "/web/palette?q=")).html;
  assert.match(all, /data-action="new-thread"/);
  assert.match(all, /href="\/\?pane=approvals"/);
  assert.match(all, /href="\/settings"/);
});

test("shortTime labels recent and older activity compactly", () => {
  const now = new Date(2026, 10, 28, 12, 0).getTime();
  assert.equal(shortTime(now - 10_000, now), "now");
  assert.equal(shortTime(now - 5 * 60_000, now), "5m");
  assert.equal(shortTime(now - 3 * 3600_000, now), "3h");
  assert.equal(shortTime(new Date(2026, 10, 25, 9).getTime(), now), "Wed");
  assert.equal(shortTime(new Date(2026, 8, 3).getTime(), now), "Sep 3");
  assert.equal(shortTime(new Date(2025, 0, 9).getTime(), now), "Jan 9, 2025");
});
