import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runModel } from "../runner.mjs";
test("cancellation kills a pending provider call promptly and removes its private files", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "runner-cancel-"));
  const bin = join(dir, "fake-codex");
  await writeFile(
    bin,
    "#!/usr/bin/env node\nprocess.stdin.resume(); setInterval(()=>{},10000);\n",
    { mode: 0o700 },
  );
  const prior = process.env.CODEX_CLI;
  process.env.CODEX_CLI = bin;
  t.after(async () => {
    if (prior === undefined) delete process.env.CODEX_CLI;
    else process.env.CODEX_CLI = prior;
    await rm(dir, { recursive: true, force: true });
  });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 100);
  const start = Date.now();
  const result = await runModel(
    {
      model: "gpt-6-astra",
      goal: "fixture",
      hosts: ["fixture.test"],
      actions: 0,
      evidence: [],
    },
    { stateDirectory: dir, socket: join(dir, "unused"), token: "synthetic" },
    controller.signal,
  );
  clearTimeout(timer);
  assert.equal(result.cancelled, true);
  assert.ok(Date.now() - start < 5000);
  assert.deepEqual(await readdir(dir), ["fake-codex"]);
});
test("missing native provider is a typed unavailable result and leaves no private files", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "runner-unavailable-"));
  const prior = process.env.CODEX_CLI;
  process.env.CODEX_CLI = join(dir, "absent-codex");
  t.after(async () => {
    if (prior === undefined) delete process.env.CODEX_CLI;
    else process.env.CODEX_CLI = prior;
    await rm(dir, { recursive: true, force: true });
  });
  await assert.rejects(
    runModel(
      {
        model: "gpt-6-astra",
        goal: "fixture",
        hosts: ["fixture.test"],
        actions: 0,
        evidence: [],
      },
      { stateDirectory: dir, socket: join(dir, "unused"), token: "synthetic" },
      new AbortController().signal,
    ),
    /model_unavailable/,
  );
  assert.deepEqual(await readdir(dir), []);
});
