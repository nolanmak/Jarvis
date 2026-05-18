// #49 — webhook signature verification tests. Pins that the Express HMAC
// matches the Rust augmentagent-channel-linear primitive (same RFC 4231
// vector), and that bad signatures are rejected with 401.

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const path = require("node:path");
const crypto = require("node:crypto");

process.env.LINEAR_WEBHOOK_SECRET = "linsek";
process.env.NOTION_WEBHOOK_SECRET = "notsek";
process.env.CALENDLY_WEBHOOK_SECRET = "calsek";

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
