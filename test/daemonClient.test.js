const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const client = require("../dist/web/daemonClient");
const { DaemonError, httpStatusFor } = require("../dist/web/errors");

const SOCKET_ENV = "AUGMENTAGENT_WEB_SOCKET";

function tempDir() {
  // Short base dir: Unix socket paths are capped near 104 bytes on macOS.
  return fs.mkdtempSync(path.join(os.platform() === "darwin" ? "/tmp" : os.tmpdir(), "dc-"));
}

// Fake daemon: reads one newline-terminated JSON request, hands it to `reply`,
// which returns a string to write (or null to stay silent).
async function fakeDaemon(t, reply) {
  const dir = tempDir();
  const socketPath = path.join(dir, "web.sock");
  const requests = [];
  const server = net.createServer((conn) => {
    let buf = "";
    conn.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      const nl = buf.indexOf("\n");
      if (nl < 0) return;
      const req = JSON.parse(buf.slice(0, nl));
      requests.push(req);
      const out = reply(req, conn);
      if (out !== null) conn.end(out);
    });
    conn.on("error", () => {});
  });
  await new Promise((resolve) => server.listen(socketPath, resolve));
  fs.chmodSync(socketPath, 0o600);
  t.after(() => {
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return { socketPath, requests };
}

function withEnv(t, value) {
  const prev = process.env[SOCKET_ENV];
  if (value === undefined) delete process.env[SOCKET_ENV];
  else process.env[SOCKET_ENV] = value;
  t.after(() => {
    if (prev === undefined) delete process.env[SOCKET_ENV];
    else process.env[SOCKET_ENV] = prev;
  });
}

async function rejectsWith(promise, code, status) {
  await assert.rejects(promise, (err) => {
    assert.ok(err instanceof DaemonError, `expected DaemonError, got ${err}`);
    assert.equal(err.code, code);
    assert.equal(httpStatusFor(err.code), status);
    return true;
  });
}

const line = (obj) => JSON.stringify(obj) + "\n";

test("every daemon error code maps to its spec HTTP status", async (t) => {
  const table = {
    bad_request: 400,
    thread_not_found: 404, action_not_found: 404, turn_not_found: 404,
    thread_archived: 409, turn_conflict: 409, already_resolved: 409,
    text_too_large: 413,
    thread_busy: 429,
    unsupported_version: 500, internal: 500, some_future_code: 500,
  };
  for (const [code, status] of Object.entries(table)) {
    const daemon = await fakeDaemon(t, () => line({ ok: false, code, error: `${code} text` }));
    withEnv(t, daemon.socketPath);
    await rejectsWith(client.turnCancel({ turn_id: "t1" }), code, status);
  }
  assert.equal(httpStatusFor("jarvis_offline"), 503);
  assert.equal(httpStatusFor("web_disabled"), 503);
  assert.equal(httpStatusFor("jarvis_timeout"), 504);
});

test("DaemonError carries the daemon's human text", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: false, code: "thread_busy", error: "3 turns running" }));
  withEnv(t, daemon.socketPath);
  await assert.rejects(client.turnSubmit({ thread_id: "th", turn_id: "t", text: "hi" }),
    (err) => err.code === "thread_busy" && err.message === "3 turns running");
});

test("unset socket env throws web_disabled without touching the filesystem", async (t) => {
  withEnv(t, undefined);
  await rejectsWith(client.threadCreate({}), "web_disabled", 503);
});

test("missing socket file is jarvis_offline", async (t) => {
  const dir = tempDir();
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  withEnv(t, path.join(dir, "nope.sock"));
  await rejectsWith(client.threadCreate({}), "jarvis_offline", 503);
});

test("a socket file with no listener (ECONNREFUSED) is jarvis_offline", async (t) => {
  const dir = tempDir();
  const socketPath = path.join(dir, "stale.sock");
  // A child binds the socket and exits without closing, leaving a dead socket file.
  const child = spawnSync(process.execPath, ["-e",
    `require("net").createServer().listen(${JSON.stringify(socketPath)}, () => process.exit(0))`]);
  assert.equal(child.status, 0);
  assert.ok(fs.lstatSync(socketPath).isSocket());
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  withEnv(t, socketPath);
  await rejectsWith(client.threadCreate({}), "jarvis_offline", 503);
});

test("connect that never completes within the connect timeout is jarvis_offline", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: true }));
  withEnv(t, daemon.socketPath);
  const started = Date.now();
  await rejectsWith(
    client.threadCreate({}, { connectTimeoutMs: 50, connect: () => new net.Socket() }),
    "jarvis_offline", 503);
  assert.ok(Date.now() - started < 2000);
});

test("no response within the response timeout is jarvis_timeout", async (t) => {
  const daemon = await fakeDaemon(t, () => null);
  withEnv(t, daemon.socketPath);
  await rejectsWith(client.threadCreate({}, { responseTimeoutMs: 50 }), "jarvis_timeout", 504);
});

test("default timeouts: 2 s connect, 10 s response, 60 s for action.resolve", () => {
  assert.equal(client.CONNECT_TIMEOUT_MS, 2000);
  assert.equal(client.RESPONSE_TIMEOUT_MS, 10000);
  assert.equal(client.ACTION_RESOLVE_TIMEOUT_MS, 60000);
  assert.equal(client.MAX_RESPONSE_BYTES, 256 * 1024);
});

test("a regular file at the socket path is refused before connecting", async (t) => {
  const dir = tempDir();
  const file = path.join(dir, "web.sock");
  fs.writeFileSync(file, "");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  withEnv(t, file);
  let connected = false;
  await rejectsWith(
    client.threadCreate({}, { connect: () => { connected = true; return new net.Socket(); } }),
    "internal", 500);
  assert.equal(connected, false);
});

test("a socket owned by another UID is refused before connecting", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: true, thread: {} }));
  withEnv(t, daemon.socketPath);
  let connected = false;
  await rejectsWith(
    client.threadCreate({}, {
      uid: process.getuid() + 1,
      connect: () => { connected = true; return new net.Socket(); },
    }),
    "internal", 500);
  assert.equal(connected, false);
});

test("a symlink to the socket is refused", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: true, thread: {} }));
  const link = daemon.socketPath + ".lnk";
  fs.symlinkSync(daemon.socketPath, link);
  withEnv(t, link);
  await rejectsWith(client.threadCreate({}), "internal", 500);
});

test("a response over 256 KiB is refused as internal", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: true, pad: "x".repeat(300 * 1024) }));
  withEnv(t, daemon.socketPath);
  await rejectsWith(client.threadCreate({}), "internal", 500);
});

test("malformed or truncated responses are internal", async (t) => {
  for (const out of ["not json\n", '{"ok":true', line({ ok: false }), line([1])]) {
    const daemon = await fakeDaemon(t, () => out);
    withEnv(t, daemon.socketPath);
    await rejectsWith(client.threadCreate({}), "internal", 500);
  }
});

test("each op sends one versioned newline-terminated request and returns the result", async (t) => {
  const thread = { id: "th1", title: "New thread", pinned: false, archived: false };
  const daemon = await fakeDaemon(t, (req) => {
    switch (req.op) {
      case "thread.create": case "thread.rename": case "thread.set": case "thread.reset_session":
        return line({ ok: true, thread });
      case "turn.submit": case "turn.cancel":
        return line({ ok: true, turn_id: req.turn_id, status: "queued" });
      case "action.resolve":
        return line({ ok: true, outcome: { kind: "manual_send_ready", summary: "s", text: "t", link: "l" } });
      default:
        return line({ ok: false, code: "bad_request", error: "unknown op" });
    }
  });
  withEnv(t, daemon.socketPath);

  assert.deepEqual(await client.threadCreate({ title: "x" }), { thread });
  assert.deepEqual(await client.threadRename({ thread_id: "th1", title: "y" }), { thread });
  assert.deepEqual(await client.threadSet({ thread_id: "th1", pinned: true }), { thread });
  assert.deepEqual(await client.threadResetSession({ thread_id: "th1" }), { thread });
  assert.deepEqual(await client.turnSubmit({ thread_id: "th1", turn_id: "tu1", text: "hello" }),
    { turn_id: "tu1", status: "queued" });
  assert.deepEqual(await client.turnCancel({ turn_id: "tu1" }), { turn_id: "tu1", status: "queued" });
  assert.deepEqual(await client.actionResolve({ action_id: "a1", verb: "approve" }),
    { outcome: { kind: "manual_send_ready", summary: "s", text: "t", link: "l" } });

  assert.deepEqual(daemon.requests, [
    { v: 1, op: "thread.create", title: "x" },
    { v: 1, op: "thread.rename", thread_id: "th1", title: "y" },
    { v: 1, op: "thread.set", thread_id: "th1", pinned: true },
    { v: 1, op: "thread.reset_session", thread_id: "th1" },
    { v: 1, op: "turn.submit", thread_id: "th1", turn_id: "tu1", text: "hello" },
    { v: 1, op: "turn.cancel", turn_id: "tu1" },
    { v: 1, op: "action.resolve", action_id: "a1", verb: "approve" },
  ]);
});

test("caller args cannot override v or op", async (t) => {
  const daemon = await fakeDaemon(t, () => line({ ok: true, turn_id: "t", status: "cancelled" }));
  withEnv(t, daemon.socketPath);
  await client.turnCancel({ turn_id: "t", v: 9, op: "thread.create" });
  assert.deepEqual(daemon.requests[0], { v: 1, op: "turn.cancel", turn_id: "t" });
});
