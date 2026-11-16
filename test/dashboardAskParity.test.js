const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const express = require("express");

test("dashboard ask uses the shared Jarvis harness without separate LLM credentials", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "jarvis-dashboard-ask-"));
  const cli = path.join(root, "fake-augmentagent");
  const argsFile = path.join(root, "args.json");
  const wiki = path.join(root, "wiki");
  fs.mkdirSync(wiki);
  fs.writeFileSync(cli, `#!/usr/bin/env node
const fs = require("node:fs");
fs.writeFileSync(process.env.SYNTHETIC_ARGS_FILE, JSON.stringify({ args: process.argv.slice(2), stdin: fs.readFileSync(0, "utf8") }));
process.stdout.write("SHARED_HARNESS_OK\\n");
`, { mode: 0o700 });
  process.env.AUGMENTAGENT_BIN = cli;
  process.env.AUGMENTAGENT_WIKI_DIR = wiki;
  process.env.SYNTHETIC_ARGS_FILE = argsFile;
  process.env.AUGMENTAGENT_API_KEY = "synthetic-dashboard-key";
  delete process.env.CEREBRAS_API_KEY;
  delete process.env.GROQ_API_KEY;

  const router = require("../dist/dashboard").default;
  const app = express();
  app.use(express.json());
  app.use(router);
  const server = app.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  try {
    const response = await fetch(`http://127.0.0.1:${server.address().port}/api/ask`, {
      method: "POST",
      headers: { Authorization: "Bearer synthetic-dashboard-key", "Content-Type": "application/json" },
      body: JSON.stringify({ question: "find the synthetic note" }),
    });
    const body = await response.text();
    assert.equal(response.status, 200, body);
    assert.deepEqual(JSON.parse(body), { answer: "SHARED_HARNESS_OK" });
    assert.deepEqual(JSON.parse(fs.readFileSync(argsFile, "utf8")), {
      args: ["--wiki-dir", wiki, "wiki", "ask", "--stdin"],
      stdin: "find the synthetic note",
    });
  } finally {
    await new Promise((resolve) => server.close(resolve));
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("closing a dashboard ask stops its Jarvis CLI process", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "jarvis-dashboard-cancel-"));
  const cli = path.join(root, "fake-augmentagent");
  const pidFile = path.join(root, "pid");
  fs.writeFileSync(cli, `#!/usr/bin/env node
const fs = require("node:fs");
const pidFile = process.env.SYNTHETIC_PID_FILE;
// Existence is the reader's ready signal: publish only the complete PID.
fs.writeFileSync(pidFile + ".tmp", String(process.pid));
fs.renameSync(pidFile + ".tmp", pidFile);
setInterval(() => {}, 1000);
`, { mode: 0o700 });
  process.env.AUGMENTAGENT_BIN = cli;
  process.env.SYNTHETIC_PID_FILE = pidFile;
  process.env.AUGMENTAGENT_API_KEY = "synthetic-dashboard-key";
  const router = require("../dist/dashboard").default;
  const app = express();
  app.use(express.json());
  app.use(router);
  const server = app.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  const controller = new AbortController();
  try {
    const request = fetch(`http://127.0.0.1:${server.address().port}/api/ask`, {
      method: "POST", signal: controller.signal,
      headers: { Authorization: "Bearer synthetic-dashboard-key", "Content-Type": "application/json" },
      body: JSON.stringify({ question: "wait for cancellation" }),
    }).then(() => null, (error) => error);
    for (let tries = 0; tries < 100 && !fs.existsSync(pidFile); tries++) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert(fs.existsSync(pidFile), "the shared CLI should have started");
    const pid = Number(fs.readFileSync(pidFile, "utf8"));
    assert(Number.isSafeInteger(pid) && pid > 0, "readiness must name a positive CLI PID, never process group 0");
    controller.abort();
    assert(await request instanceof Error);
    let gone = false;
    for (let tries = 0; tries < 100; tries++) {
      try { process.kill(pid, 0); } catch { gone = true; break; }
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert(gone, "disconnect must stop the CLI process");
  } finally {
    controller.abort();
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("dashboard cancellation never observes a partially published fixture PID", { timeout: 15000 }, async () => {
  if (process.platform === "win32") return;
  const { spawnSync } = require("node:child_process");
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "jarvis-dashboard-pid-race-"));
  const gate = path.join(root, "gate");
  const trace = path.join(root, "trace.jsonl");
  try {
    const fifo = spawnSync("mkfifo", [gate], { encoding: "utf8" });
    assert.equal(fifo.status, 0, fifo.stderr);
    const preload = path.join(__dirname, "fixtures/dashboardPidPublication.cjs");
    const { promisify } = require("node:util");
    const { execFile } = require("node:child_process");
    const env = { ...process.env, L5_PID_GATE: gate, L5_PID_TRACE: trace,
      NODE_OPTIONS: `${process.env.NODE_OPTIONS || ""} --require=${JSON.stringify(preload)}` };
    delete env.NODE_TEST_CONTEXT; // A separate runner, not recursion in this worker.
    let output;
    try {
      output = await promisify(execFile)(process.execPath, ["--test",
        "--test-name-pattern=^closing a dashboard ask stops its Jarvis CLI process$", __filename], {
        env,
        timeout: 10000,
      });
    } catch (error) {
      assert.fail(`${error.stdout || ""}\n${error.stderr || ""}\n${fs.existsSync(trace) ? fs.readFileSync(trace, "utf8") : "no lifecycle trace"}`);
    }
    assert(fs.existsSync(trace), JSON.stringify(output));
    const events = fs.readFileSync(trace, "utf8").trim().split("\n").map(JSON.parse);
    assert(events.some((event) => event.event === "pid-file-created"));
    assert(events.some((event) => event.event === "pid-not-published"),
      "the reader must encounter the paused writer before its PID is published");
    const read = events.find((event) => event.event === "pid-read");
    const spawned = events.find((event) => event.event === "spawn");
    assert.equal(read.parsed, spawned.child, "the readiness file must name the actual child");
    assert(events.some((event) => event.event === "child-exit" && event.signal === "SIGTERM"));
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
