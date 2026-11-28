const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");
const express = require("express");
const Database = require("better-sqlite3");
const { initDb, getDb } = require("../dist/db");
const { ReadDb } = require("../dist/web/readDb");
const { ChangeWatcher } = require("../dist/web/changeWatcher");
const { createWebRoutes } = require("../dist/web/routes");

const SOCKET_ENV = "AUGMENTAGENT_WEB_SOCKET";

// Copied from spec §5.1 (docs/specs/web-dashboard-v2.md). The source of truth is
// crates/augmentagent-store/src/web_chat.rs; keep this in step with it.
const WEB_DDL = `
CREATE TABLE web_threads(id TEXT PRIMARY KEY, title TEXT NOT NULL,
  title_set INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, changed_at INTEGER NOT NULL,
  pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0);
CREATE TABLE web_chat_messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,
  thread_id TEXT NOT NULL REFERENCES web_threads(id), turn_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('owner','jarvis')), text TEXT NOT NULL,
  created_at INTEGER NOT NULL, UNIQUE(turn_id, role));
CREATE TABLE web_turns(turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES web_threads(id),
  text_sha256 TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('queued','running','done','failed','cancelled')),
  error_code TEXT NULL, error TEXT NULL, created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, finished_at INTEGER NULL);
CREATE INDEX web_turns_updated ON web_turns(updated_at);
`;

// One temp DB for the file: initDb is a process-wide singleton.
const dir = fs.mkdtempSync(path.join(os.platform() === "darwin" ? "/tmp" : os.tmpdir(), "wr-"));
const dbPath = path.join(dir, "data.db");
initDb(dbPath);
test.after(() => fs.rmSync(dir, { recursive: true, force: true }));

function uuidv7() {
  const b = crypto.randomBytes(16);
  b.writeUIntBE(Date.now(), 0, 6);
  b[6] = (b[6] & 0x0f) | 0x70;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = b.toString("hex");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

function createWebTables() {
  const w = new Database(dbPath);
  w.exec(WEB_DDL);
  return w;
}

function dropWebTables() {
  const w = new Database(dbPath);
  w.exec("DROP TABLE IF EXISTS web_chat_messages; DROP TABLE IF EXISTS web_turns; DROP TABLE IF EXISTS web_threads;");
  w.close();
}

function addThread(w, id, fields = {}) {
  const now = fields.updated_at ?? Date.now();
  w.prepare(
    `INSERT INTO web_threads(id,title,created_at,updated_at,changed_at,pinned,archived)
     VALUES (?,?,?,?,?,?,?)`,
  ).run(id, fields.title ?? "New thread", now, now, now, fields.pinned ?? 0, fields.archived ?? 0);
}

// Fake daemon on a Unix socket: one JSON request per connection, `reply(req)`
// returns the response object.
async function fakeDaemon(t, reply) {
  const sockDir = fs.mkdtempSync(path.join(os.platform() === "darwin" ? "/tmp" : os.tmpdir(), "wd-"));
  const socketPath = path.join(sockDir, "web.sock");
  const requests = [];
  const server = net.createServer((conn) => {
    let buf = "";
    conn.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      const nl = buf.indexOf("\n");
      if (nl < 0) return;
      const req = JSON.parse(buf.slice(0, nl));
      requests.push(req);
      conn.end(JSON.stringify(reply(req)) + "\n");
    });
    conn.on("error", () => {});
  });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  fs.chmodSync(socketPath, 0o600);
  const prev = process.env[SOCKET_ENV];
  process.env[SOCKET_ENV] = socketPath;
  t.after(() => {
    if (prev === undefined) delete process.env[SOCKET_ENV];
    else process.env[SOCKET_ENV] = prev;
    server.close();
    fs.rmSync(sockDir, { recursive: true, force: true });
  });
  return { socketPath, requests };
}

async function startApp(t, opts = {}) {
  const readDb = new ReadDb(dbPath);
  const watcher = new ChangeWatcher(readDb, { intervalMs: 20 });
  watcher.start();
  const app = express();
  app.use(express.json());
  app.use(createWebRoutes({ readDb: () => readDb, watcher: () => watcher, heartbeatMs: opts.heartbeatMs }));
  const server = app.listen(0, "127.0.0.1");
  await new Promise((r) => server.once("listening", r));
  t.after(() => {
    watcher.stop();
    server.closeAllConnections();
    server.close();
    readDb.close();
  });
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (method, url, body) => {
    const res = await fetch(base + url, {
      method,
      headers: body === undefined ? {} : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: res.status, body: await res.json() };
  };
  return { base, call, watcher };
}

// Reads SSE frames from /api/web/events until `predicate` matches one.
async function readEvents(base, predicate, timeoutMs = 3000) {
  const ac = new AbortController();
  const res = await fetch(base + "/api/web/events", { signal: ac.signal });
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type"), /text\/event-stream/);
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  const frames = [];
  let buf = "";
  const timer = setTimeout(() => ac.abort(), timeoutMs);
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let idx;
      while ((idx = buf.indexOf("\n\n")) >= 0) {
        const raw = buf.slice(0, idx);
        buf = buf.slice(idx + 2);
        const frame = {};
        for (const l of raw.split("\n")) {
          if (l.startsWith("event: ")) frame.event = l.slice(7);
          else if (l.startsWith("data: ")) frame.data = l.slice(6);
          else if (l.startsWith(":")) frame.comment = l.slice(1).trim();
        }
        frames.push(frame);
        if (predicate(frame)) return frames;
      }
    }
  } catch (err) {
    if (err.name !== "AbortError") throw err;
  } finally {
    clearTimeout(timer);
    ac.abort();
  }
  return frames;
}

test("missing web tables read as empty lists", async (t) => {
  dropWebTables();
  const { call } = await startApp(t);
  assert.deepEqual((await call("GET", "/api/web/threads")).body, { threads: [] });
  assert.deepEqual((await call("GET", "/api/web/threads?archived=1")).body, { threads: [] });
  const msgs = await call("GET", "/api/web/threads/nope/messages");
  assert.equal(msgs.status, 404);
  assert.equal(msgs.body.code, "thread_not_found");
});

test("threads list sorts pinned first, then updated_at desc, filtered by archived", async (t) => {
  dropWebTables();
  const w = createWebTables();
  addThread(w, "a", { updated_at: 1000 });
  addThread(w, "b", { updated_at: 3000 });
  addThread(w, "c", { updated_at: 500, pinned: 1 });
  addThread(w, "d", { updated_at: 9000, archived: 1 });
  w.close();
  const { call } = await startApp(t);
  const live = (await call("GET", "/api/web/threads")).body.threads;
  assert.deepEqual(live.map((th) => th.id), ["c", "b", "a"]);
  assert.equal(live[0].pinned, true);
  assert.equal(live[1].archived, false);
  assert.deepEqual(Object.keys(live[0]).sort(), ["archived", "changed_at", "created_at", "id", "pinned", "title", "updated_at"]);
  const archived = (await call("GET", "/api/web/threads?archived=1")).body.threads;
  assert.deepEqual(archived.map((th) => th.id), ["d"]);
});

test("submit returns 202, the watcher emits changed, and a retry gets the same result", async (t) => {
  dropWebTables();
  const w = createWebTables();
  t.after(() => w.close());
  const threadId = uuidv7();
  addThread(w, threadId);
  // Fake daemon: writes rows like turn.submit does (owner message + queued turn).
  const daemon = await fakeDaemon(t, (req) => {
    assert.equal(req.op, "turn.submit");
    const existing = w.prepare("SELECT status FROM web_turns WHERE turn_id = ?").get(req.turn_id);
    if (existing) return { ok: true, turn_id: req.turn_id, status: existing.status };
    const now = Date.now();
    const sha = crypto.createHash("sha256").update(req.text).digest("hex");
    w.prepare("INSERT INTO web_chat_messages(thread_id,turn_id,role,text,created_at) VALUES (?,?,?,?,?)")
      .run(req.thread_id, req.turn_id, "owner", req.text, now);
    w.prepare("INSERT INTO web_turns(turn_id,thread_id,text_sha256,status,created_at,updated_at) VALUES (?,?,?,?,?,?)")
      .run(req.turn_id, req.thread_id, sha, "queued", now, now);
    w.prepare("UPDATE web_threads SET updated_at = ?, changed_at = ? WHERE id = ?").run(now, now, req.thread_id);
    return { ok: true, turn_id: req.turn_id, status: "queued" };
  });
  const { base, call } = await startApp(t);

  const events = readEvents(base, (f) => f.event === "changed");
  await new Promise((r) => setTimeout(r, 50)); // let the SSE client attach
  const turnId = uuidv7();
  const first = await call("POST", `/api/web/threads/${threadId}/turns`, { turn_id: turnId, text: "hello" });
  assert.equal(first.status, 202);
  assert.deepEqual(first.body, { turn_id: turnId, status: "queued" });

  const frames = await events;
  const changed = frames.find((f) => f.event === "changed");
  assert.ok(changed, `no changed event in ${JSON.stringify(frames)}`);
  assert.deepEqual(JSON.parse(changed.data), { thread_id: threadId });

  const retry = await call("POST", `/api/web/threads/${threadId}/turns`, { turn_id: turnId, text: "hello" });
  assert.equal(retry.status, 202);
  assert.deepEqual(retry.body, first.body);
  assert.equal(daemon.requests.length, 2);
  assert.deepEqual(daemon.requests[0], { v: 1, op: "turn.submit", thread_id: threadId, turn_id: turnId, text: "hello" });

  const log = await call("GET", `/api/web/threads/${threadId}/messages`);
  assert.equal(log.status, 200);
  assert.equal(log.body.messages.length, 1);
  assert.deepEqual(Object.keys(log.body.messages[0]).sort(), ["created_at", "role", "seq", "text", "turn_id"]);
  assert.deepEqual(log.body.turns.map((x) => [x.turn_id, x.status]), [[turnId, "queued"]]);
  assert.deepEqual(Object.keys(log.body.turns[0]).sort(), ["created_at", "error", "error_code", "finished_at", "status", "turn_id"]);
});

test("the watcher also emits threads_changed when a thread row changes", async (t) => {
  dropWebTables();
  const w = createWebTables();
  t.after(() => w.close());
  const { base } = await startApp(t);
  const events = readEvents(base, (f) => f.event === "threads_changed");
  await new Promise((r) => setTimeout(r, 50));
  addThread(w, uuidv7(), { updated_at: Date.now() + 5 });
  const frames = await events;
  assert.ok(frames.some((f) => f.event === "threads_changed"), JSON.stringify(frames));
});

test("SSE sends a heartbeat comment", async (t) => {
  const { base } = await startApp(t, { heartbeatMs: 30 });
  const frames = await readEvents(base, (f) => f.comment === "heartbeat");
  assert.ok(frames.some((f) => f.comment === "heartbeat"));
});

test("messages paginate with before/limit and include non-terminal turns", async (t) => {
  dropWebTables();
  const w = createWebTables();
  addThread(w, "t1");
  const ins = w.prepare("INSERT INTO web_chat_messages(thread_id,turn_id,role,text,created_at) VALUES (?,?,?,?,?)");
  const turn = w.prepare("INSERT INTO web_turns(turn_id,thread_id,text_sha256,status,created_at,updated_at,finished_at) VALUES (?,?,?,?,?,?,?)");
  for (let i = 1; i <= 5; i += 1) {
    ins.run("t1", `u${i}`, "owner", `m${i}`, i);
    turn.run(`u${i}`, "t1", "x", "done", i, i, i);
  }
  turn.run("u9", "t1", "x", "running", 9, 9, null); // no message yet in this window
  w.close();
  const { call } = await startApp(t);
  const page = await call("GET", "/api/web/threads/t1/messages?limit=2");
  assert.deepEqual(page.body.messages.map((m) => m.text), ["m4", "m5"]);
  assert.deepEqual(page.body.turns.map((x) => x.turn_id).sort(), ["u4", "u5", "u9"]);
  const older = await call("GET", `/api/web/threads/t1/messages?before=${page.body.messages[0].seq}&limit=500`);
  assert.deepEqual(older.body.messages.map((m) => m.text), ["m1", "m2", "m3"]);
  assert.equal((await call("GET", "/api/web/threads/t1/messages?limit=0")).status, 400);
  assert.equal((await call("GET", "/api/web/threads/t1/messages?before=abc")).status, 400);
});

test("Node rejects bad turn input before calling the socket", async (t) => {
  const daemon = await fakeDaemon(t, () => ({ ok: true, turn_id: "x", status: "queued" }));
  const { call } = await startApp(t);
  const big = await call("POST", "/api/web/threads/t1/turns", { turn_id: uuidv7(), text: "é".repeat(16 * 1024 + 1) });
  assert.equal(big.status, 413);
  assert.equal(big.body.code, "text_too_large");
  const badId = await call("POST", "/api/web/threads/t1/turns", { turn_id: "not-a-uuid", text: "hi" });
  assert.equal(badId.status, 400);
  const empty = await call("POST", "/api/web/threads/t1/turns", { turn_id: uuidv7(), text: "   " });
  assert.equal(empty.status, 400);
  const badVerb = await call("POST", "/api/web/approvals/a1", { verb: "explode" });
  assert.equal(badVerb.status, 400);
  const reviseNoNote = await call("POST", "/api/web/approvals/a1", { verb: "revise" });
  assert.equal(reviseNoNote.status, 400);
  assert.equal((await call("PATCH", "/api/web/threads/t1", {})).status, 400);
  assert.equal(daemon.requests.length, 0);
});

test("daemon error codes map to HTTP statuses", async (t) => {
  let code = "internal";
  await fakeDaemon(t, () => ({ ok: false, code, error: `${code} text` }));
  const { call } = await startApp(t);
  const cases = { text_too_large: 413, thread_not_found: 404, turn_conflict: 409, thread_archived: 409, thread_busy: 429 };
  for (const [c, status] of Object.entries(cases)) {
    code = c;
    const res = await call("POST", "/api/web/threads/t1/turns", { turn_id: uuidv7(), text: "hi" });
    assert.equal(res.status, status, c);
    assert.deepEqual(res.body, { code: c, error: `${c} text` });
  }
  code = "turn_not_found";
  assert.equal((await call("POST", `/api/web/turns/${uuidv7()}/cancel`)).status, 404);
  code = "already_resolved";
  assert.equal((await call("POST", "/api/web/approvals/a1", { verb: "approve" })).status, 409);
});

test("an absent daemon socket is 503 jarvis_offline; an unset socket env is 503 web_disabled", async (t) => {
  const prev = process.env[SOCKET_ENV];
  t.after(() => {
    if (prev === undefined) delete process.env[SOCKET_ENV];
    else process.env[SOCKET_ENV] = prev;
  });
  const { call } = await startApp(t);
  process.env[SOCKET_ENV] = path.join(dir, "missing.sock");
  const offline = await call("POST", "/api/web/threads", {});
  assert.equal(offline.status, 503);
  assert.equal(offline.body.code, "jarvis_offline");
  delete process.env[SOCKET_ENV];
  const disabled = await call("POST", "/api/web/threads", {});
  assert.equal(disabled.status, 503);
  assert.equal(disabled.body.code, "web_disabled");
});

test("thread writes proxy to the daemon ops", async (t) => {
  const thread = { id: "t1", title: "T", created_at: 1, updated_at: 1, changed_at: 1, pinned: false, archived: false };
  const daemon = await fakeDaemon(t, (req) => {
    if (req.op === "turn.cancel") return { ok: true, turn_id: req.turn_id, status: "cancelled" };
    return { ok: true, thread };
  });
  const { call } = await startApp(t);
  const created = await call("POST", "/api/web/threads", { title: "T" });
  assert.equal(created.status, 201);
  assert.deepEqual(created.body, { thread });
  assert.equal((await call("PATCH", "/api/web/threads/t1", { title: "New", pinned: true })).status, 200);
  assert.equal((await call("POST", "/api/web/threads/t1/reset-session")).status, 200);
  const turnId = uuidv7();
  const cancel = await call("POST", `/api/web/turns/${turnId}/cancel`);
  assert.deepEqual(cancel.body, { turn_id: turnId, status: "cancelled" });
  assert.deepEqual(
    daemon.requests.map(({ v, ...r }) => r),
    [
      { op: "thread.create", title: "T" },
      { op: "thread.rename", thread_id: "t1", title: "New" },
      { op: "thread.set", thread_id: "t1", pinned: true },
      { op: "thread.reset_session", thread_id: "t1" },
      { op: "turn.cancel", turn_id: turnId },
    ],
  );
});

test("approvals list pending actions newest first and resolve through the daemon", async (t) => {
  const db = getDb();
  const insert = db.prepare(
    `INSERT INTO actions(id,messageId,fromEmail,subject,draftBody,status,createdAt,updatedAt)
     VALUES (?,?,?,?,?,?,?,?)`,
  );
  insert.run("act-old", "m-old", "a@example.com", "Old", "d", "pending", 1000, 1000);
  insert.run("act-new", "m-new", "b@example.com", "New", "d", "pending", 2000, 2000);
  insert.run("act-done", "m-done", "c@example.com", "Done", "d", "approved", 3000, 3000);
  const daemon = await fakeDaemon(t, () => ({ ok: true, outcome: { kind: "sent", summary: "Sent to b" } }));
  const { call } = await startApp(t);
  const list = await call("GET", "/api/web/approvals");
  assert.equal(list.status, 200);
  assert.deepEqual(list.body.actions.map((a) => a.id), ["act-new", "act-old"]);
  const resolved = await call("POST", "/api/web/approvals/act-new", { verb: "revise", note: "shorter" });
  assert.equal(resolved.status, 200);
  assert.deepEqual(resolved.body, { outcome: { kind: "sent", summary: "Sent to b" } });
  const { v, ...req } = daemon.requests[0];
  assert.deepEqual(req, { op: "action.resolve", action_id: "act-new", verb: "revise", note: "shorter" });
});

test("watcher re-emits the boundary row only after a new commit", async (t) => {
  dropWebTables();
  const w = createWebTables();
  t.after(() => w.close());
  addThread(w, "t1", { updated_at: 100 });
  const readDb = new ReadDb(dbPath);
  t.after(() => readDb.close());
  const watcher = new ChangeWatcher(readDb, { now: () => 50 });
  const seen = [];
  watcher.on("event", (ev) => seen.push(ev.type === "changed" ? ev.thread_id : ev.type));
  const turn = w.prepare("INSERT INTO web_turns(turn_id,thread_id,text_sha256,status,created_at,updated_at) VALUES (?,?,?,?,?,?)");
  turn.run("u1", "t1", "x", "queued", 200, 200);
  watcher.tick();
  assert.deepEqual(seen, ["t1", "threads_changed"]);
  watcher.tick(); // no commit since: nothing re-emitted
  assert.deepEqual(seen, ["t1", "threads_changed"]);
  addThread(w, "t2", { updated_at: 100 }); // a commit below both cursors
  turn.run("u2", "t2", "x", "queued", 200, 200); // same millisecond as the boundary
  watcher.tick();
  assert.deepEqual(seen.slice(2).sort(), ["t1", "t2", "threads_changed"]);
});
