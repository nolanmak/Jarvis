// #49 — webhook signature verification tests. Pins that the Express HMAC
// matches the Rust augmentagent-channel-linear primitive (same RFC 4231
// vector), and that bad signatures are rejected with 401.

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");
const crypto = require("node:crypto");

process.env.LINEAR_WEBHOOK_SECRET = "linsek";
process.env.NOTION_WEBHOOK_SECRET = "notsek";
process.env.CALENDLY_WEBHOOK_SECRET = "calsek";
process.env.SOCIALAPI_WEBHOOK_SECRET = "soapisek";

const express = require("express");
const webhooksRouter = require(path.join(
  __dirname,
  "..",
  "dist",
  "webhooks.js"
)).default;
const db = require(path.join(__dirname, "..", "dist", "db.js"));

// The socialapi receiver persists to the shared SQLite — init a throwaway DB
// so getDb()/insertSocialApiWebhookEvent have a target.
const tmpDb = path.join(
  fs.mkdtempSync(path.join(os.tmpdir(), "aa-webhook-test-")),
  "data.db"
);
db.initDb(tmpDb);

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
        res.on("end", () => resolve({ status: res.statusCode, body: b }));
      }
    );
    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

function hmacHex(secret, body) {
  return crypto.createHmac("sha256", secret).update(body).digest("hex");
}

test("linear hmac matches RFC4231 vector (parity with Rust)", () => {
  // RFC 4231 case 2 — identical to the Rust crate's hmac_rfc4231_vector test.
  assert.strictEqual(
    hmacHex("Jefe", "what do ya want for nothing?"),
    "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
  );
});

test("linear webhook accepts good sig, rejects bad", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({ data: { id: "iss1" } });
    const good = await post(port, "/webhooks/linear", body, {
      "content-type": "application/json",
      "linear-signature": hmacHex("linsek", body),
    });
    assert.strictEqual(good.status, 202);
    const bad = await post(port, "/webhooks/linear", body, {
      "content-type": "application/json",
      "linear-signature": "deadbeef",
    });
    assert.strictEqual(bad.status, 401);
  } finally {
    s.close();
  }
});

test("notion accepts static token or hmac", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({ page: { id: "p1" } });
    const tok = await post(port, "/webhooks/notion", body, {
      "content-type": "application/json",
      "notion-webhook-secret": "notsek",
    });
    assert.strictEqual(tok.status, 202);
    const sig = await post(port, "/webhooks/notion", body, {
      "content-type": "application/json",
      "x-notion-signature": hmacHex("notsek", body),
    });
    assert.strictEqual(sig.status, 202);
    const bad = await post(port, "/webhooks/notion", body, {
      "content-type": "application/json",
      "notion-webhook-secret": "wrong",
    });
    assert.strictEqual(bad.status, 401);
  } finally {
    s.close();
  }
});

// #249 — SocialAPI.ai inbound webhook receiver.
test("socialapi webhook persists verified events idempotently, rejects bad/unsigned", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({
      events: [
        {
          type: "dm",
          id: "m1",
          conversation_id: "conv_1",
          account_id: "acc_1",
          with: "jane",
          author: "jane",
          text: "hey there",
          created_at: "2026-05-28T00:00:00Z",
        },
        {
          type: "comment",
          comment_id: "c1",
          post_id: "post_1",
          author: "bob",
          text: "nice!",
          created_at: "2026-05-28T00:01:00Z",
        },
      ],
    });
    const good = await post(port, "/webhooks/socialapi", body, {
      "content-type": "application/json",
      "x-socialapi-signature": hmacHex("soapisek", body),
    });
    assert.strictEqual(good.status, 202);
    assert.deepStrictEqual(JSON.parse(good.body), {
      ok: true,
      accepted: 2,
      duplicates: 0,
      skipped: 0,
    });

    // Re-deliver the SAME body → both events de-dup by id (provider retry).
    const dup = await post(port, "/webhooks/socialapi", body, {
      "content-type": "application/json",
      "x-socialapi-signature": hmacHex("soapisek", body),
    });
    assert.strictEqual(dup.status, 202);
    assert.deepStrictEqual(JSON.parse(dup.body), {
      ok: true,
      accepted: 0,
      duplicates: 2,
      skipped: 0,
    });

    // The rows are durably in socialapi_webhook_events, unprocessed.
    const rows = db
      .getDb()
      .prepare(
        "SELECT id, kind, processed FROM socialapi_webhook_events ORDER BY id"
      )
      .all();
    assert.strictEqual(rows.length, 2);
    assert.ok(rows.every((r) => r.processed === 0));

    // Bad signature → 401, nothing persisted beyond the 2 above.
    const bad = await post(port, "/webhooks/socialapi", body, {
      "content-type": "application/json",
      "x-socialapi-signature": "deadbeef",
    });
    assert.strictEqual(bad.status, 401);

    // sha256=<hex> framing is also accepted.
    const framedBody = JSON.stringify({
      type: "dm",
      id: "m2",
      conversation_id: "conv_2",
      author: "amy",
      text: "ping",
    });
    const framed = await post(port, "/webhooks/socialapi", framedBody, {
      "content-type": "application/json",
      "x-socialapi-signature": "sha256=" + hmacHex("soapisek", framedBody),
    });
    assert.strictEqual(framed.status, 202);
    assert.strictEqual(JSON.parse(framed.body).accepted, 1);
  } finally {
    s.close();
  }
});

test("socialapi post mentions are not misclassified as DMs", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({
      type: "message",
      id: "mention_1",
      conversation_id: "thread_for_post",
      post_id: "post_1",
      comment_id: "comment_1",
      platform: "instagram",
      author: "block_space_phl",
      text: "@phillyteche",
    });
    const res = await post(port, "/webhooks/socialapi", body, {
      "content-type": "application/json",
      "x-socialapi-signature": hmacHex("soapisek", body),
    });
    assert.strictEqual(res.status, 202);
    assert.strictEqual(JSON.parse(res.body).accepted, 1);
    const row = db
      .getDb()
      .prepare("SELECT kind FROM socialapi_webhook_events WHERE id = ?")
      .get("socialapi:comment:post_1:comment_1");
    assert.strictEqual(row.kind, "comment");
  } finally {
    s.close();
  }
});

test("socialapi webhook fails closed when no secret configured", async () => {
  const prev = process.env.SOCIALAPI_WEBHOOK_SECRET;
  delete process.env.SOCIALAPI_WEBHOOK_SECRET;
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({ type: "dm", id: "x", conversation_id: "c" });
    // Even a "correctly signed against empty" request is rejected — fail-closed.
    const res = await post(port, "/webhooks/socialapi", body, {
      "content-type": "application/json",
      "x-socialapi-signature": hmacHex("", body),
    });
    assert.strictEqual(res.status, 401);
  } finally {
    s.close();
    if (prev !== undefined) process.env.SOCIALAPI_WEBHOOK_SECRET = prev;
  }
});

test("calendly verifies t=,v1= scheme", async () => {
  const s = await startServer();
  const port = s.address().port;
  try {
    const body = JSON.stringify({ event: "invitee.created" });
    const t = "1700000000";
    const v1 = hmacHex("calsek", Buffer.concat([Buffer.from(`${t}.`), Buffer.from(body)]));
    const ok = await post(port, "/webhooks/calendly", body, {
      "content-type": "application/json",
      "calendly-webhook-signature": `t=${t},v1=${v1}`,
    });
    assert.strictEqual(ok.status, 202);
    const bad = await post(port, "/webhooks/calendly", body, {
      "content-type": "application/json",
      "calendly-webhook-signature": "t=1,v1=bad",
    });
    assert.strictEqual(bad.status, 401);
  } finally {
    s.close();
  }
});
