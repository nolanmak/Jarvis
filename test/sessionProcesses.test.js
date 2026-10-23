const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn, spawnSync } = require("node:child_process");
const { createMacBackend, createSessionManager, parseMacPsRows } = require("../dist/sessionProcesses");

function row(overrides = {}) {
  return {
    pid: 221, ppid: 1, etime: "00:03", tty: "ttys001", cwd: "",
    cmd: "/opt/Claude Tools/claude", started: "Mon Sep 29 03:00:00 2026",
    executable: "/opt/Claude Tools/claude", uid: 501, ...overrides,
  };
}

test("macOS ps parser preserves paths with spaces and ignores args mentioning claude", () => {
  const output = [
    " 221 1 00:03 ttys001 501 Mon Sep 29 03:00:00 2026 /opt/Claude Tools/claude",
    " 222 1 00:03 ?? 501 Mon Sep 29 03:00:00 2026 /usr/bin/node --flag claude",
    " 223 1 00:03 ?? 502 Mon Sep 29 03:00:00 2026 /opt/Claude Tools/claude",
    " 224 1 00:03 ?? 501 Mon Sep 29 03:00:00 2026 /Users/Test User/café/claude",
  ].join("\n");
  const rows = parseMacPsRows(output, 501);
  assert.equal(rows.length, 2);
  assert.deepEqual(rows[0], row({ pid: 224, tty: "??", cmd: "/Users/Test User/café/claude",
                                  executable: "/Users/Test User/café/claude" }));
  assert.deepEqual(rows[1], row());
});

test("stop checks owner, executable and birth time immediately before signaling", () => {
  let current = row();
  const signals = [];
  const manager = createSessionManager({
    list: () => [current], inspect: () => current,
    signal: (pid, signal) => signals.push([pid, signal]),
    currentUid: () => 501,
  });
  assert.equal(manager.stop(221, "old birth", false).status, 409);
  current = row({ uid: 502 });
  assert.equal(manager.stop(221, current.started, false).status, 403);
  current = row({ executable: "/usr/bin/node", cmd: "node claude" });
  assert.equal(manager.stop(221, current.started, false).status, 403);
  current = row({ pid: 222 });
  assert.equal(manager.stop(221, current.started, false).status, 409);
  current = null;
  assert.equal(manager.stop(221, row().started, false).status, 404);
  assert.deepEqual(signals, []);
  current = row();
  assert.deepEqual(manager.stop(221, current.started, false),
                   { status: 200, body: { ok: true, pid: 221, signal: "SIGTERM" } });
  assert.deepEqual(manager.stop(221, current.started, true),
                   { status: 200, body: { ok: true, pid: 221, signal: "SIGKILL" } });
  assert.deepEqual(signals, [[221, "SIGTERM"], [221, "SIGKILL"]]);
  for (const [code, status] of [["ESRCH", 404], ["EPERM", 403]]) {
    const failure = createSessionManager({
      list: () => [current], inspect: () => current, currentUid: () => 501,
      signal: () => { throw Object.assign(new Error(code), { code }); },
    });
    assert.equal(failure.stop(221, current.started, false).status, status);
  }
});

test("macOS ps timeout is distinct from an empty process list", () => {
  const timedOut = createMacBackend(() => { throw new Error("ps timed out"); }, () => 501);
  assert.throws(() => timedOut.list(), /inspection unavailable/);
  assert.equal(timedOut.inspect(221), null);
  const empty = createMacBackend(() => "", () => 501);
  assert.deepEqual(empty.list(), []);
});

// A fixture must be a REAL executable whose kernel-owned `comm` is "claude":
// both backends identify a session by `comm`, not by argv0, so a shell script
// wrapping sleep is reported as "sleep" and correctly ignored.
//
// Copying a system binary no longer works on macOS. /bin/sleep is an arm64e
// platform binary, and a copy loses the signature that entitles it, so the
// kernel SIGKILLs it on exec (exit 137) — the fixture died before `ps` ever saw
// it, which is why this test failed on every Mac. Hardlinking is refused too
// ("Operation not permitted"). Compiling a two-line binary is the only approach
// that yields a genuine executable under a chosen name.
function buildClaudeFixture(scratch) {
  const executable = path.join(scratch, "claude");
  const compiler = ["cc", "clang", "gcc"].find((candidate) => {
    const probe = spawnSync(candidate, ["--version"], { stdio: "ignore" });
    return !probe.error && probe.status === 0;
  });
  if (!compiler) return null;
  const source = path.join(scratch, "claude.c");
  fs.writeFileSync(source, "#include <unistd.h>\nint main(void){ for(;;) pause(); return 0; }\n");
  const built = spawnSync(compiler, ["-O0", "-o", executable, source], { stdio: "ignore" });
  if (built.status !== 0) return null;
  fs.chmodSync(executable, 0o700);
  return executable;
}

test("native host discovers and stops only a disposable claude-named process", async (t) => {
  if (!(["linux", "darwin"].includes(process.platform))) return;
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "jarvis-session-process-"));
  const executable = buildClaudeFixture(scratch);
  if (!executable) {
    fs.rmSync(scratch, { recursive: true, force: true });
    // Skipped rather than silently passing: without a compiler there is no way
    // to produce a real executable named "claude" to discover.
    t.skip("no C compiler available to build the claude-named fixture");
    return;
  }
  const manager = createSessionManager();
  try {
    for (const force of [false, true]) {
      const child = spawn(executable, { stdio: "ignore" });
      try {
        await new Promise((resolve, reject) => {
          child.once("spawn", resolve);
          child.once("error", reject);
        });
        let found;
        for (let attempt = 0; attempt < 50; attempt++) {
          found = manager.list().find((candidate) => candidate.pid === child.pid);
          if (found) break;
          await new Promise((resolve) => setTimeout(resolve, 50));
        }
        assert(found, "the fixture must be discoverable on the native host");
        assert.equal(found.uid, process.getuid());
        assert(Number.isSafeInteger(found.ppid) && found.ppid > 0);
        assert(found.etime.length > 0, "elapsed time must be present");
        assert(found.tty.length > 0, "TTY field must be present");
        assert.equal(manager.stop(child.pid, found.started, force).status, 200);
        const exited = await Promise.race([
          new Promise((resolve) => child.once("exit", () => resolve(true))),
          new Promise((resolve) => setTimeout(() => resolve(false), 2000)),
        ]);
        assert(exited, force ? "SIGKILL should stop the fixture" : "SIGTERM should stop the fixture");
      } finally {
        if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      }
    }
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
});

test("Linux ignores a non-Claude executable with a forged argv0", async () => {
  if (process.platform !== "linux") return;
  const child = spawn("bash", ["-c", "exec -a claude /bin/sleep 30"], { stdio: "ignore" });
  const manager = createSessionManager();
  try {
    await new Promise((resolve, reject) => { child.once("spawn", resolve); child.once("error", reject); });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(manager.list().some((candidate) => candidate.pid === child.pid), false);
  } finally {
    child.kill("SIGKILL");
  }
});

test("sessions stop route keeps dashboard authentication and identity gates", async () => {
  process.env.AUGMENTAGENT_API_KEY = "synthetic-dashboard-key";
  const express = require("express");
  const router = require("../dist/dashboard").default;
  const app = express();
  app.use(express.json());
  app.use(router);
  const server = app.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  const url = `http://127.0.0.1:${server.address().port}/api/sessions/1/stop`;
  try {
    const unauthorized = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
    assert.equal(unauthorized.status, 401);
    const authorized = await fetch(url, {
      method: "POST",
      headers: { authorization: "Bearer synthetic-dashboard-key", "content-type": "application/json" },
      body: JSON.stringify({ started: "synthetic", force: true }),
    });
    assert.equal(authorized.status, 400);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});
