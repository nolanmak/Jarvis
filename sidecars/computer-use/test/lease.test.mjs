import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp, mkdir, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { connect } from "node:net";

const server = fileURLToPath(new URL("../start.sh", import.meta.url));
const python = execFileSync("python3", ["-c", "import sys; print(sys.executable)"], { encoding: "utf8" }).trim();

test("worker starts without a flock executable on PATH", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "computer-lease-"));
  const emptyPath = join(scratch, "empty-bin");
  await mkdir(emptyPath);
  const socket = join(scratch, "worker.sock");
  const child = spawn("/bin/bash", [server], {
    cwd: fileURLToPath(new URL("..", import.meta.url)),
    env: {
      ...process.env, PATH: emptyPath,
      JARVIS_COMPUTER_PYTHON: python,
      JARVIS_COMPUTER_NODE: process.execPath,
      JARVIS_COMPUTER_STATE: scratch,
      JARVIS_COMPUTER_SOCKET: socket,
      JARVIS_COMPUTER_CHROME_BRIDGE: join(scratch, "missing-bridge"),
      JARVIS_COMPUTER_LOCK_DIRECTORY: scratch,
    },
    stdio: "ignore",
  });
  try {
    let ready = false;
    for (let attempt = 0; attempt < 60; attempt++) {
      try { await stat(socket); ready = true; break; } catch { /* wait */ }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert(ready, "worker should bind its private socket without the Linux flock command");
  } finally {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGTERM");
      await new Promise((resolve) => child.once("close", resolve));
    }
    await rm(scratch, { recursive: true, force: true });
  }
});

test("only one worker owns the socket, and a crashed holder can be replaced", async () => {
  const scratch = await mkdtemp(join(tmpdir(), "computer-singleton-"));
  const socket = join(scratch, "worker.sock");
  const env = {
    ...process.env,
    JARVIS_COMPUTER_STATE: scratch,
    JARVIS_COMPUTER_SOCKET: socket,
    JARVIS_COMPUTER_CHROME_BRIDGE: join(scratch, "missing-bridge"),
    JARVIS_COMPUTER_LOCK_DIRECTORY: scratch,
  };
  const children = [];
  const start = () => {
    const child = spawn("/bin/bash", [server], { env, stdio: "ignore" });
    children.push(child);
    return child;
  };
  const closed = (child) => child.exitCode !== null || child.signalCode !== null
    ? Promise.resolve(child.exitCode)
    : new Promise((resolve) => child.once("close", resolve));
  const reachable = () => new Promise((resolve) => {
    const client = connect(socket);
    client.once("connect", () => { client.destroy(); resolve(true); });
    client.once("error", () => resolve(false));
  });
  const waitFor = async (predicate) => {
    for (let i = 0; i < 80; i++) {
      if (await predicate()) return true;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return false;
  };
  try {
    const first = start();
    assert(await waitFor(reachable), "first worker should become reachable");
    const loser = start();
    assert.notEqual(await closed(loser), 0, "second worker must lose the lease");
    assert(await reachable(), "loser must not unlink the live socket");
    first.kill("SIGKILL");
    await closed(first);
    const replacement = start();
    assert(await waitFor(reachable), "replacement should recover the stale socket");
    assert.equal(replacement.exitCode, null);
  } finally {
    for (const child of children) {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGTERM");
        await closed(child);
      }
    }
    await rm(scratch, { recursive: true, force: true });
  }
});
