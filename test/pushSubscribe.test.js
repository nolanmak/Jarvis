// #45 — PWA Web Push subscription endpoint test. Asserts subscribe persists
// to the shared pwa_subscriptions table (same one the Rust store uses) and
// that malformed bodies are rejected.

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

const tmpDb = path.join(os.tmpdir(), `aa-push-${process.pid}.db`);
process.env.AUGMENTAGENT_DB = tmpDb;
process.env.MODE = "local";
delete process.env.AUGMENTAGENT_API_KEY;

const express = require("express");
const { initDb, listPushSubscriptions } = require(path.join(
  __dirname,
  "..",
  "dist",
  "db.js"
));
const apiV1Router = require(path.join(__dirname, "..", "dist", "apiV1.js"))
  .default;

function start() {
  initDb(tmpDb);
  const app = express();
  app.use(express.json());
  app.use(apiV1Router);
  return new Promise((r) => {
    const s = app.listen(0, () => r(s));
  });
}

function post(port, p, body) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        port,
        path: p,
        method: "POST",
        headers: { "content-type": "application/json" },
      },
      (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () => resolve({ status: res.statusCode, body: b }));
      }
    );
    req.on("error", reject);
    req.write(JSON.stringify(body));
    req.end();
  });
}

test.after(() => {
  for (const ext of ["", "-wal", "-shm"]) {
    try {
      fs.unlinkSync(tmpDb + ext);
    } catch {}
  }
});

test("push subscribe persists, rejects malformed", async () => {
  const s = await start();
  const port = s.address().port;
  try {
    const good = await post(port, "/api/push/subscribe", {
      endpoint: "https://push.example/abc",
      keys: { p256dh: "p", auth: "a" },
    });
    assert.strictEqual(good.status, 201);
    const subs = listPushSubscriptions();
    assert.strictEqual(subs.length, 1);
    assert.strictEqual(subs[0].endpoint, "https://push.example/abc");

    const bad = await post(port, "/api/push/subscribe", { endpoint: "x" });
    assert.strictEqual(bad.status, 400);
  } finally {
    s.close();
  }
});
