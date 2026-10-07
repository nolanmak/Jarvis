// #1412 — the Rust daemon drops `actions.originalBody` from old terminal
// actions when it only repeats `emails.body`. The dashboard must keep showing
// the original message for those rows, and must not invent a body elsewhere.
//
// Run: node --test test/actionBodyCompaction.test.js   (after `npm run build`)

const test = require("node:test");
const assert = require("node:assert");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

const tmpDb = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "aa-compaction-")), "t.db");
process.env.AUGMENTAGENT_DB = tmpDb;

const { initDb, getDb, getActions, getActionById } = require(path.join(__dirname, "..", "dist", "db.js"));

function seed(id, status, originalBody, emailBody) {
  const db = getDb();
  db.prepare(
    "INSERT INTO emails (messageId, fromEmail, subject, body, firstSeenAt) VALUES (?, 'a@example.test', 's', ?, 0)"
  ).run(`m-${id}`, emailBody);
  db.prepare(
    "INSERT INTO actions (id, messageId, fromEmail, subject, originalBody, status, createdAt, updatedAt) VALUES (?, ?, 'a@example.test', 's', ?, ?, 0, 0)"
  ).run(id, `m-${id}`, originalBody, status);
}

test.before(() => {
  initDb(tmpDb);
  seed("compacted", "skipped", null, "the original message");
  seed("intact", "skipped", "its own copy", "the email text");
  seed("pending-no-body", "pending", null, "the email text");
});

test.after(() => {
  fs.rmSync(path.dirname(tmpDb), { recursive: true, force: true });
});

test("a compacted terminal action reads its body from the email", () => {
  assert.strictEqual(getActionById("compacted").originalBody, "the original message");
  const listed = getActions({ limit: 10 }).find((a) => a.id === "compacted");
  assert.strictEqual(listed.originalBody, "the original message");
});

test("an action that still has its own body is unchanged", () => {
  assert.strictEqual(getActionById("intact").originalBody, "its own copy");
});

test("an action that can still act and never had a body does not gain one", () => {
  assert.equal(getActionById("pending-no-body").originalBody, null);
});

test("the helper column never leaks into the record", () => {
  assert.ok(!("compactedBody" in getActionById("compacted")));
  assert.ok(getActions({ limit: 10 }).every((a) => !("compactedBody" in a)));
});
