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
    fromEmail: "a@b.example.com",
    subject: "first",
    originalBody: "hi",
    draftBody: "draft1",
    status: "pending",
    errorMessage: null,
  });
  logAction({
    messageId: "m2",
    threadId: null,
    fromEmail: "c@d.example.com",
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

function post(port, p, body, headers = {}) {
  const data = JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        port,
        path: p,
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Content-Length": Buffer.byteLength(data),
          ...headers,
        },
      },
      (res) => {
        let out = "";
        res.on("data", (c) => (out += c));
        res.on("end", () => resolve({ status: res.statusCode, body: out }));
      }
    );
    req.on("error", reject);
    req.end(data);
  });
}

// #2707 (spec D5): Node must never flip an action's status by SQL. A web
// "Approve" that only rewrote the row recorded a send that never happened.
// The legacy route keeps field edits on a pending action and refuses any
// other target status with use_real_approval.
for (const target of ["sent", "rejected", "skipped", "approved", "failed"]) {
  test(`POST /api/v1/actions/:id refuses status=${target} and leaves the row pending`, async () => {
    const srv = await startServer();
    const port = srv.address().port;
    try {
      const { getActionById } = require(path.join(__dirname, "..", "dist", "db.js"));
      const id = getActions({ limit: 20, offset: 0, status: "pending" })[0].id;
      const res = await post(
        port,
        `/api/v1/actions/${id}`,
        { status: target, source: "web_push" },
        { "x-api-key": "test-key-123" }
      );
      assert.strictEqual(res.status, 409);
      assert.deepStrictEqual(JSON.parse(res.body), { error: "use_real_approval" });
      assert.strictEqual(getActionById(id).status, "pending");
    } finally {
      srv.close();
    }
  });
}

test("POST /api/v1/actions/:id still accepts a pending recipient edit (draft-preview)", async () => {
  const srv = await startServer();
  const port = srv.address().port;
  try {
    const { getActionById } = require(path.join(__dirname, "..", "dist", "db.js"));
    const id = getActions({ limit: 20, offset: 0, status: "pending" })[0].id;
    const res = await post(
      port,
      `/api/v1/actions/${id}`,
      { status: "pending", recipientEmail: "new@x.example.com", source: "dashboard_preview" },
      { "x-api-key": "test-key-123" }
    );
    assert.strictEqual(res.status, 200);
    const json = JSON.parse(res.body);
    assert.strictEqual(json.status, "pending");
    assert.strictEqual(json.recipientEmail, "new@x.example.com");
    assert.strictEqual(getActionById(id).recipientEmail, "new@x.example.com");
  } finally {
    srv.close();
  }
});

test("legacy /queue view is read-only: no Approve, Skip or Revise", () => {
  const view = fs.readFileSync(path.join(__dirname, "..", "views", "queue.ejs"), "utf8");
  assert.doesNotMatch(view, />\s*(Approve|Skip|Revise)\s*</);
  assert.doesNotMatch(view, /method:\s*"POST"[\s\S]*\/api\/v1\/actions\//);
  assert.doesNotMatch(view, /fetch\("\/api\/v1\/actions\/"/);
});
