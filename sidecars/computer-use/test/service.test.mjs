import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rpc } from "../rpc.mjs";
test("service rejects forged callers, preserves owner scope and keeps state private", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "computer-service-"));
  const socket = join(dir, "worker.sock");
  const child = spawn(process.execPath, ["server.mjs"], {
    env: {
      ...process.env,
      JARVIS_COMPUTER_STATE: dir,
      JARVIS_COMPUTER_SOCKET: socket,
      JARVIS_COMPUTER_CHROME_BRIDGE: join(dir, "no-chrome"),
      JARVIS_COMPUTER_LOCK_DIRECTORY: dir,
    },
    stdio: "ignore",
  });
  t.after(async () => {
    child.kill("SIGTERM");
    await new Promise((r) => child.once("close", r));
    await rm(dir, { recursive: true, force: true });
  });
  let token;
  for (let i = 0; i < 100; i++) {
    try {
      token = (await readFile(join(dir, "token"), "utf8")).trim();
      await stat(socket);
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 50));
    }
  }
  assert.ok(token);
  await assert.rejects(
    rpc(socket, "forged", {
      operation: "start",
      owner: "owner",
      request: "e",
      goal: "test",
      hosts: ["example.com"],
    }),
    /unauthorized/,
  );
  const body = {
    operation: "start",
    owner: "owner",
    request: "event1",
    goal: "Read example.com",
    hosts: ["example.com"],
  };
  const task = await rpc(socket, token, body);
  assert.equal((await rpc(socket, token, body)).id, task.id);
  await assert.rejects(
    rpc(socket, token, {
      operation: "status",
      owner: "other",
      request: "e",
      taskId: task.id,
    }),
    /not_found/,
  );
  const status = await rpc(socket, token, {
    operation: "status",
    owner: "owner",
    request: "e",
    taskId: task.id,
  });
  assert.equal(status.status, "needs_action");
  assert.equal(status.reason, "chrome_debugging_required");
  assert.equal((await stat(join(dir, "tasks.json"))).mode & 0o777, 0o600);
});
