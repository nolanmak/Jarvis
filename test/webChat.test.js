// #2718 — Jarvis Dashboard message log, composer, turn pills, cancel/reset
// (spec §5.4, D12). Render tests with fixture threads for each turn status.

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
const { createFragmentRoutes } = require("../dist/web/fragments");

const tmpRoot = os.platform() === "darwin" ? "/tmp" : os.tmpdir();
const dir = fs.mkdtempSync(path.join(tmpRoot, "wc-"));
const dbPath = path.join(dir, "data.db");
initDb(dbPath);
test.after(() => fs.rmSync(dir, { recursive: true, force: true }));

const TURN = (n) => `0193a000-0000-7000-8000-${String(n).padStart(12, "0")}`;

{
  const w = new Database(dbPath);
  w.exec(`
    CREATE TABLE web_threads(id TEXT PRIMARY KEY, title TEXT NOT NULL,
      title_set INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL, changed_at INTEGER NOT NULL,
      pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE web_chat_messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,
      thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, role TEXT NOT NULL, text TEXT NOT NULL,
      created_at INTEGER NOT NULL, UNIQUE(turn_id, role));
    CREATE TABLE web_turns(turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, text_sha256 TEXT NOT NULL,
      status TEXT NOT NULL, error_code TEXT NULL, error TEXT NULL, created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL, finished_at INTEGER NULL);`);
  const now = Date.now();
  const thread = w.prepare("INSERT INTO web_threads(id,title,created_at,updated_at,changed_at,archived) VALUES (?,?,?,?,?,?)");
  thread.run("t1", "Status fixtures", now, now, now, 0);
  thread.run("t-empty", "Fresh thread", now, now, now, 0);
  thread.run("t-arc", "Archived thread", now, now, now, 1);
  const msg = w.prepare("INSERT INTO web_chat_messages(thread_id,turn_id,role,text,created_at) VALUES ('t1',?,?,?,?)");
  const turn = w.prepare(
    "INSERT INTO web_turns(turn_id,thread_id,text_sha256,status,error_code,error,created_at,updated_at) VALUES (?,'t1','x',?,?,?,?,?)",
  );
  const rows = [
    [1, "done", null, null, "What is on today?", "**Two** meetings and <script>alert(1)</script>"],
    [2, "failed", "provider_error", "The model provider returned 529.", "Retry that", null],
    [3, "failed", "uncertain_session", "The session state is uncertain.", "Then this", null],
    [4, "cancelled", null, null, "Never mind", null],
    [5, "running", null, null, "Summarize my inbox", null],
    [6, "queued", null, null, "And then the calendar", null],
  ];
  for (const [n, status, code, error, owner, jarvis] of rows) {
    turn.run(TURN(n), status, code, error, now + n, now + n);
    msg.run(TURN(n), "owner", owner, now + n);
    if (jarvis) msg.run(TURN(n), "jarvis", jarvis, now + n);
  }
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
  process.env.AUGMENTAGENT_WEB_SOCKET = socketPath;
  t.after(() => {
    delete process.env.AUGMENTAGENT_WEB_SOCKET;
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

// The HTML for one turn's row: from its owner message to the next one.
function turnBlock(html, n) {
  const start = html.indexOf(`data-turn="${TURN(n)}"`);
  assert.ok(start >= 0, `turn ${n} rendered`);
  const next = html.indexOf('class="j-msg j-msg-owner"', start + 1);
  return html.slice(start, next < 0 ? undefined : next);
}

test("the log renders every turn status with its pill, actions and errors", async (t) => {
  const call = await startApp(t);
  const res = await call("GET", "/web/threads/t1/log");
  assert.equal(res.status, 200);
  const html = res.html;
  assert.match(html, /^<div id="log"[^>]*hx-get="\/web\/threads\/t1\/log"[^>]*hx-trigger="jarvis:refresh"/);

  const done = turnBlock(html, 1);
  assert.match(done, /<strong>Two<\/strong> meetings and &lt;script&gt;/);
  assert.ok(!done.includes("<script>"));
  assert.ok(!/j-pill/.test(done), "done turns have no pill");

  const failed = turnBlock(html, 2);
  assert.match(failed, /j-pill-failed/);
  assert.match(failed, /The model provider returned 529\./);
  assert.ok(!/Reset session/.test(failed));

  const uncertain = turnBlock(html, 3);
  assert.match(uncertain, /j-pill-failed/);
  assert.match(uncertain, /hx-post="\/web\/threads\/t1\/reset-session"[^>]*>[\s\S]*Reset session/);

  const cancelled = turnBlock(html, 4);
  assert.match(cancelled, /j-pill-cancelled/);
  assert.ok(!/Cancel</.test(cancelled));

  for (const n of [5, 6]) {
    const block = turnBlock(html, n);
    assert.match(block, new RegExp(`j-pill-${n === 5 ? "running" : "queued"}`));
    assert.match(block, new RegExp(`hx-post="/web/turns/${TURN(n)}/cancel"`));
  }
  // Owner messages are escaped plain text, never markdown.
  assert.ok(html.indexOf("What is on today?") < html.indexOf("Summarize my inbox"));
});

test("an empty thread shows a prompt, and unknown threads 404 into the toast", async (t) => {
  const call = await startApp(t);
  assert.match((await call("GET", "/web/threads/t-empty/log")).html, /Ask Jarvis anything/);
  const missing = await call("GET", "/web/threads/nope/log");
  assert.equal(missing.status, 404);
  assert.equal(missing.headers.get("hx-retarget"), "#j-toast");
});

test("/?thread=<id> renders the log and a composer that never zooms; archived threads cannot send", async (t) => {
  const call = await startApp(t);
  const html = (await call("GET", "/?thread=t1")).html;
  assert.ok(html.includes('<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">'));
  assert.match(html, /<nav class="j-tabbar"/);
  assert.match(html, /<div id="log"/);
  assert.match(html, /<form id="composer"[^>]*hx-post="\/web\/threads\/t1\/turns"/);
  assert.match(html, /<textarea[^>]*name="text"/);
  assert.match(html, /<input type="hidden" name="turn_id"/);
  const archived = (await call("GET", "/?thread=t-arc")).html;
  assert.match(archived, /Unarchive this thread to send/);
  assert.ok(!/<form id="composer"/.test(archived));
});

test("submit, cancel and reset call the daemon and return the log", async (t) => {
  const requests = await fakeDaemon(t, (req) =>
    req.op === "thread.reset_session"
      ? { ok: true, thread: { id: "t1", title: "x", created_at: 1, updated_at: 1, changed_at: 1, pinned: false, archived: false } }
      : { ok: true, turn_id: req.turn_id, status: "queued" },
  );
  const call = await startApp(t);
  const turnId = TURN(99);
  const sent = await call("POST", "/web/threads/t1/turns", { turn_id: turnId, text: "hello" });
  assert.equal(sent.status, 200);
  assert.match(sent.html, /^<div id="log"/);
  assert.deepEqual(requests[0], { v: 1, op: "turn.submit", thread_id: "t1", turn_id: turnId, text: "hello" });

  const cancelled = await call("POST", `/web/turns/${TURN(5)}/cancel`, { thread_id: "t1" });
  assert.equal(cancelled.status, 200);
  assert.equal(requests[1].op, "turn.cancel");
  assert.match(cancelled.html, /^<div id="log"/);

  const reset = await call("POST", "/web/threads/t1/reset-session");
  assert.equal(reset.status, 200);
  assert.equal(requests[2].op, "thread.reset_session");
});

test("composer errors render inline with the code for the turn_id retention rule", async (t) => {
  await fakeDaemon(t, () => ({ ok: false, code: "thread_busy", error: "busy" }));
  const call = await startApp(t);
  const res = await call("POST", "/web/threads/t1/turns", { turn_id: TURN(98), text: "hi" });
  assert.equal(res.status, 429);
  assert.equal(res.headers.get("hx-retarget"), "#composer-error");
  assert.equal(res.headers.get("x-jarvis-error"), "thread_busy");
  assert.match(res.html, /still working on this thread/);

  const bad = await call("POST", "/web/threads/t1/turns", { turn_id: "not-a-uuid", text: "hi" });
  assert.equal(bad.status, 400);
  assert.equal(bad.headers.get("x-jarvis-error"), "bad_request");
});
