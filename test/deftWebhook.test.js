// #116 — Deft (Deftform) webhook receiver tests.
//
// Deftform webhooks are UNSIGNED (no HMAC documented — docs/deft-protocol.md
// §4), so the only auth is a shared secret in the URL path. These tests pin:
//   - valid submission with the right secret → 202 + spooled
//   - duplicate submission (same uuid) → 202 but counted as duplicate, not
//     re-spooled
//   - wrong secret → 401, nothing spooled
//   - path inert unless AUGMENTAGENT_DEFT_ENABLED is truthy
// The seen-set + spool files are redirected to a per-run temp path so the
// test is hermetic.

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const path = require("node:path");
const fs = require("node:fs");
const os = require("node:os");

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "deft-wh-"));
process.env.AUGMENTAGENT_WEBHOOK_SPOOL = path.join(TMP, "spool.jsonl");
process.env.AUGMENTAGENT_DEFT_SEEN = path.join(TMP, "seen.jsonl");
process.env.AUGMENTAGENT_DEFT_WEBHOOK_SECRET = "s3cr3t-path-token";
process.env.AUGMENTAGENT_DEFT_ENABLED = "1";

const express = require("express");
const webhooksRouter = require(path.join(
  __dirname,
  "..",
  "dist",
  "webhooks.js"
)).default;

function startServer() {
  const app = express();
  app.use(webhooksRouter);
  return new Promise((r) => {
    const s = app.listen(0, () => r(s));
  });
}

function post(port, p, body, headers) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { port, path: p, method: "POST", headers },
      (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () =>
          resolve({ status: res.statusCode, body: b })
        );
      }
    );
    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

function spoolLines() {
  try {
    return fs
      .readFileSync(process.env.AUGMENTAGENT_WEBHOOK_SPOOL, "utf8")
      .split("\n")
      .filter(Boolean)
      .map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

const SUBMISSION = JSON.stringify({
  submission_id: "s-1",
  uuid: "uuid-abc-123",
  formId: "OUm6T9",
  data: [
    {
      label: "Command",
      response: "approve",
      uuid: "f-1",
      custom_key: "agent_command",
    },
  ],
});

test("valid submission with correct secret → 202 and spooled", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const r = await post(
      port,
      "/webhooks/deft/s3cr3t-path-token",
      SUBMISSION,
      { "content-type": "application/json" }
    );
    assert.strictEqual(r.status, 202);
    const j = JSON.parse(r.body);
    assert.strictEqual(j.accepted, 1);
    assert.strictEqual(j.duplicates, 0);
    const lines = spoolLines();
    const deft = lines.filter((l) => l.platform === "deft");
    assert.strictEqual(deft.length, 1);
    assert.strictEqual(deft[0].payload.uuid, "uuid-abc-123");
  } finally {
    s.close();
  }
});

test("duplicate submission (same uuid) is not re-spooled", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    // First delivery (the seen-set already has uuid-abc-123 from the prior
    // test via the shared persisted file — assert dedup directly).
    const r = await post(
      port,
      "/webhooks/deft/s3cr3t-path-token",
      SUBMISSION,
      { "content-type": "application/json" }
    );
    assert.strictEqual(r.status, 202);
    const j = JSON.parse(r.body);
    assert.strictEqual(j.accepted, 0);
    assert.strictEqual(j.duplicates, 1);
    // Spool still has exactly the one line from the first test.
    const deft = spoolLines().filter((l) => l.platform === "deft");
    assert.strictEqual(deft.length, 1);
  } finally {
    s.close();
  }
});

test("wrong secret → 401, nothing spooled", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const before = spoolLines().filter((l) => l.platform === "deft").length;
    const r = await post(
      port,
      "/webhooks/deft/WRONG-secret",
      JSON.stringify({ uuid: "uuid-should-not-spool", data: [] }),
      { "content-type": "application/json" }
    );
    assert.strictEqual(r.status, 401);
    const after = spoolLines().filter((l) => l.platform === "deft").length;
    assert.strictEqual(after, before);
  } finally {
    s.close();
  }
});

test("path inert when AUGMENTAGENT_DEFT_ENABLED is off", async () => {
  process.env.AUGMENTAGENT_DEFT_ENABLED = "off";
  // Re-require a fresh module instance so the gate is re-read? The gate is
  // read per-request from process.env, so no re-require needed.
  const s = await startServer();
  const port = s.address().port;
  try {
    const r = await post(
      port,
      "/webhooks/deft/s3cr3t-path-token",
      JSON.stringify({ uuid: "uuid-while-disabled", data: [] }),
      { "content-type": "application/json" }
    );
    assert.strictEqual(r.status, 401);
  } finally {
    s.close();
    process.env.AUGMENTAGENT_DEFT_ENABLED = "1";
  }
});

test("multiple submissions in a data[]-of-submissions frame", async () => {
  // Use a fresh seen file so these uuids are new.
  process.env.AUGMENTAGENT_DEFT_SEEN = path.join(TMP, "seen2.jsonl");
  delete require.cache[require.resolve(path.join(__dirname, "..", "dist", "webhooks.js"))];
  const router2 = require(path.join(__dirname, "..", "dist", "webhooks.js")).default;
  const app = express();
  app.use(router2);
  const s = await new Promise((r) => {
    const srv = app.listen(0, () => r(srv));
  });
  const port = s.address().port;
  try {
    const body = JSON.stringify({
      data: [
        { uuid: "wh-u1", formId: "F1", data: [{ label: "C", response: "approve", custom_key: "agent_command" }] },
        { uuid: "wh-u2", formId: "F1", data: [{ label: "C", response: "skip", custom_key: "agent_command" }] },
      ],
    });
    const r = await post(port, "/webhooks/deft/s3cr3t-path-token", body, {
      "content-type": "application/json",
    });
    assert.strictEqual(r.status, 202);
    const j = JSON.parse(r.body);
    assert.strictEqual(j.accepted, 2);
    assert.strictEqual(j.duplicates, 0);
  } finally {
    s.close();
  }
});

test("bad json → 400", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const r = await post(
      port,
      "/webhooks/deft/s3cr3t-path-token",
      "{not json",
      { "content-type": "application/json" }
    );
    assert.strictEqual(r.status, 400);
  } finally {
    s.close();
  }
});
