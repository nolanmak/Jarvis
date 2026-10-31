const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const { initDb, getDb, updateActionStatus, resolveActionIfPending, updateActionRecipient } =
  require(path.join(__dirname, "..", "dist", "db.js"));

test("dashboard mutations respect a Rust owner-text reservation and resume after release", () => {
  const db = initDb(":memory:");
  try {
    db.prepare("INSERT INTO actions(id,messageId,fromEmail,subject,draftBody,status,createdAt,updatedAt) VALUES('a','m','peer@example.invalid','test','original','pending',0,0)").run();
    // A dashboard may start before Rust creates the additive table.
    assert.equal(updateActionRecipient("a", "peer@example.invalid"), true);
    db.exec("CREATE TABLE imessage_owner_action_leases(action_id TEXT PRIMARY KEY, token TEXT)");
    db.prepare("INSERT INTO imessage_owner_action_leases VALUES('a','active-owner-event')").run();
    for (const mutate of [
      () => updateActionRecipient("a", "other@example.invalid"),
      () => updateActionStatus("a", "sent", { draftBody: "changed" }),
      () => resolveActionIfPending("a", "skipped"),
    ]) assert.throws(mutate, /unresolved owner text decision/);
    assert.deepEqual(getDb().prepare("SELECT draftBody,status,recipientEmail FROM actions WHERE id='a'").get(),
      { draftBody: "original", status: "pending", recipientEmail: "peer@example.invalid" });
    db.prepare("DELETE FROM imessage_owner_action_leases WHERE action_id='a'").run();
    assert.equal(resolveActionIfPending("a", "skipped"), true);
  } finally { db.close(); }
});
