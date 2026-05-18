// #1 — split-deployment parity test.
//
// Asserts the versioned JSON API (`/api/v1/*`) returns the same underlying
// data the HTMX dashboard renders, so a split deployment (remote UI hitting
// the JSON API) sees an identical queue. Both surfaces read db.ts; this test
// pins that they agree and that the API-key middleware behaves per MODE.
//
// Run: node --test test/apiV1.parity.test.js   (after `npm run build`)

const test = require("node:test");
const assert = require("node:assert");
const http = require("node:http");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

// Isolated temp DB so we don't touch the real data.db.
const tmpDb = path.join(os.tmpdir(), `aa-parity-${process.pid}.db`);
process.env.AUGMENTAGENT_DB = tmpDb;
process.env.MODE = "local";
process.env.AUGMENTAGENT_API_KEY = "test-key-123";

const express = require("express");
const { initDb, logAction, getActions } = require(path.join(
  __dirname,
  "..",
  "dist",
  "db.js"
));
const apiV1Router = require(path.join(__dirname, "..", "dist", "apiV1.js"))
  .default;

function seed() {
  initDb(tmpDb);
  // Two actions in pending state.
  logAction({
    messageId: "m1",
    threadId: null,
    fromEmail: "a@b.com",
    subject: "first",
    originalBody: "hi",
    draftBody: "draft1",
    status: "pending",
    errorMessage: null,
  });
  logAction({
    messageId: "m2",
    threadId: null,
    fromEmail: "c@d.com",
    subject: "second",
    originalBody: "yo",
    draftBody: "draft2",
    status: "pending",
    errorMessage: null,
  });
}

function startServer() {
  const app = express();
  app.use(express.json());
  app.use(apiV1Router);
  return new Promise((resolve) => {
    const srv = app.listen(0, () => resolve(srv));
  });
}

function get(port, p, headers = {}) {
  return new Promise((resolve, reject) => {
    http
      .get(
        { port, path: p, headers },
        (res) => {
          let body = "";
          res.on("data", (c) => (body += c));
          res.on("end", () =>
            resolve({ status: res.statusCode, body })
          );
        }
      )
      .on("error", reject);
  });
}

test.after(() => {
  for (const ext of ["", "-wal", "-shm"]) {
    try {
      fs.unlinkSync(tmpDb + ext);
    } catch {}
  }
});

test("v1 actions JSON matches db.ts getActions (parity)", async () => {
  seed();
  const srv = await startServer();
  const port = srv.address().port;
  try {
    const res = await get(port, "/api/v1/actions", {
      "x-api-key": "test-key-123",
    });
    assert.strictEqual(res.status, 200);
    const json = JSON.parse(res.body);
    const direct = getActions({ limit: 20, offset: 0 });
    assert.strictEqual(json.total, direct.length);
    assert.strictEqual(json.actions.length, direct.length);
    assert.deepStrictEqual(
      json.actions.map((a) => a.id).sort(),
      direct.map((a) => a.id).sort()
    );
  } finally {
    srv.close();
  }
});

test("API key enforced when configured", async () => {
  const srv = await startServer();
  const port = srv.address().port;
  try {
    const noKey = await get(port, "/api/v1/stats");
    assert.strictEqual(noKey.status, 401);
    const ok = await get(port, "/api/v1/stats", {
      "x-api-key": "test-key-123",
    });
    assert.strictEqual(ok.status, 200);
  } finally {
    srv.close();
  }
});

test("v1 stats parity with getStats", async () => {
  const srv = await startServer();
  const port = srv.address().port;
  try {
    const { getStats } = require(path.join(
      __dirname,
      "..",
      "dist",
      "db.js"
    ));
    const res = await get(port, "/api/v1/stats", {
      "x-api-key": "test-key-123",
    });
    assert.deepStrictEqual(JSON.parse(res.body), getStats());
  } finally {
    srv.close();
  }
});
