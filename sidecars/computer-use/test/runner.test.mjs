import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runModel, CAP } from "../runner.mjs";
function invoke(dir, signal = new AbortController().signal) {
  return runModel(
    {
      model: "gpt-6-astra",
      goal: "fixture",
      hosts: ["fixture.test"],
      actions: 0,
      evidence: [],
    },
    { stateDirectory: dir, socket: join(dir, "unused"), token: "synthetic" },
    signal,
  );
}
// A fake `codex` that emits canned --json stdout / stderr then exits with the
// chosen code, waiting for stdin to close so the parent's write never EPIPEs.
async function fakeCodex(dir, { stdout = [], stderr = [], code = 0 }) {
  const emit = (fd, items) =>
    items.map((s) => `fs.writeSync(${fd}, ${JSON.stringify(s)});`).join("\n");
  const lines = stdout.map((o) =>
    typeof o === "string" ? o : JSON.stringify(o) + "\n",
  );
  const body =
    'const fs = require("node:fs");\n' +
    emit(1, lines) +
    "\n" +
    emit(2, stderr) +
    "\n" +
    'process.stdin.on("data", () => {});\n' +
    `process.stdin.on("end", () => process.exit(${code}));\n`;
  const bin = join(dir, "fake-codex");
  await writeFile(bin, "#!/usr/bin/env node\n" + body, { mode: 0o700 });
  process.env.CODEX_CLI = bin;
}
async function setup(t, prefix) {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  const prior = process.env.CODEX_CLI;
  t.after(async () => {
    if (prior === undefined) delete process.env.CODEX_CLI;
    else process.env.CODEX_CLI = prior;
    await rm(dir, { recursive: true, force: true });
  });
  return dir;
}
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
test("missing native provider surfaces provider_error, not a bare model_unavailable, and leaves no private files", async (t) => {
  const dir = await mkdtemp(join(tmpdir(), "runner-unavailable-"));
  const prior = process.env.CODEX_CLI;
  process.env.CODEX_CLI = join(dir, "absent-codex");
  t.after(async () => {
    if (prior === undefined) delete process.env.CODEX_CLI;
    else process.env.CODEX_CLI = prior;
    await rm(dir, { recursive: true, force: true });
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "provider_error");
    return true;
  });
  assert.deepEqual(await readdir(dir), []);
});
test("a usage-limit wall is typed as usage_limit with the reset phrase, never collapsed to model_unavailable", async (t) => {
  const dir = await setup(t, "runner-usage-");
  await fakeCodex(dir, {
    stdout: [
      {
        type: "error",
        message:
          "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 24th, 2026 4:18 AM.",
      },
      { type: "turn.failed", error: { message: "turn failed" } },
    ],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "usage_limit");
    assert.notEqual(err.code, "model_unavailable");
    assert.equal(err.resetText, "Sep 24th, 2026 4:18 AM");
    assert.equal(err.resetAt, null);
    assert.match(err.message, /usage limit/);
    return true;
  });
});
test("an oversized non-ASCII terminal lastError is bounded to CAP by bytes, not code units", async (t) => {
  const dir = await setup(t, "runner-cap-");
  // A JSON error event never passes through the stderr byte cap, so its message
  // is the unbounded source. CAP is a byte budget: a usage-limit wall trailed by
  // 2,048 CJK characters (~6 KiB in UTF-8) must still surface no more than CAP
  // BYTES — a code-unit slice would pass ~2,048 chars straight through.
  await fakeCodex(dir, {
    stdout: [{ type: "error", message: "usage limit " + "中".repeat(2048) }],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "usage_limit");
    assert.ok(Buffer.byteLength(err.message, "utf8") <= CAP);
    return true;
  });
});
test("a genuinely unknown model is typed as model_unavailable", async (t) => {
  const dir = await setup(t, "runner-unknown-model-");
  await fakeCodex(dir, {
    stdout: [{ type: "error", message: "model 'gpt-6-astra' not found" }],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "model_unavailable");
    return true;
  });
});
test("a non-zero exit with no recognizable event is provider_error carrying a bounded stderr tail", async (t) => {
  const dir = await setup(t, "runner-provider-");
  await fakeCodex(dir, {
    stdout: ["this line is not json\n"],
    stderr: ["codex: fatal: worker crashed unexpectedly\n"],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "provider_error");
    assert.ok(err.detail && err.detail.length > 0);
    assert.ok(err.detail.length <= CAP);
    assert.match(err.detail, /worker crashed/);
    return true;
  });
});
test("a full-cap provider stderr tail keeps the prefixed message within CAP", async (t) => {
  const dir = await setup(t, "runner-provider-cap-");
  // A stderr tail that saturates the byte budget: the "provider_error: "
  // prefix must count against CAP, not be added on top of an already full-cap
  // detail. Regression for the prefix pushing err.message past CAP.
  await fakeCodex(dir, {
    stdout: ["this line is not json\n"],
    stderr: ["E".repeat(CAP * 2) + "\n"],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "provider_error");
    assert.ok(Buffer.byteLength(err.message, "utf8") <= CAP);
    assert.ok(Buffer.byteLength(err.detail, "utf8") <= CAP);
    return true;
  });
});
test("secrets injected into stderr are redacted from every surfaced field", async (t) => {
  const dir = await setup(t, "runner-redact-");
  const SECRET = "TOPSECRETtoken1234567890abcdefABCDEF";
  await fakeCodex(dir, {
    stderr: [`auth failed Authorization: Bearer ${SECRET}\n`],
    code: 1,
  });
  await assert.rejects(invoke(dir), (err) => {
    assert.equal(err.code, "provider_error");
    for (const field of [err.message, err.detail, err.resetText])
      assert.ok(!String(field ?? "").includes(SECRET));
    return true;
  });
});
test("a clean exit reporting turn.completed still resolves with usage", async (t) => {
  const dir = await setup(t, "runner-success-");
  await fakeCodex(dir, {
    stdout: [{ type: "turn.completed", usage: { total_tokens: 7 } }],
    code: 0,
  });
  const result = await invoke(dir);
  assert.deepEqual(result.usage, { total_tokens: 7 });
});
