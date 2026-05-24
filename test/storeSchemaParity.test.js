// #45 — Schema parity test between Node's initDb() and Rust's
// augmentagent-store::Store::migrate(). Both surfaces share the same data.db
// file at runtime; if one drifts from the other, ALTER guards or column
// references on the other side silently break.
//
// The Rust crate is the authoritative source of schema as of #45, but Node
// still creates the same 9 base tables defensively (for concurrent-startup
// scenarios). This test pins the column lists Node creates so any change
// here is forced to be matched in `crates/augmentagent-store/src/store.rs`
// (and vice versa — the matching Rust test
// `store_open_creates_all_node_owned_tables` pins table presence on that
// side).
//
// Run: node --test test/storeSchemaParity.test.js   (after `npm run build`)

const test = require("node:test");
const assert = require("node:assert");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");

const tmpDb = path.join(os.tmpdir(), `aa-schema-parity-${process.pid}.db`);
process.env.AUGMENTAGENT_DB = tmpDb;
process.env.MODE = "local";

const { initDb, getDb } = require(path.join(__dirname, "..", "dist", "db.js"));

// Column sets MUST match the CREATE TABLE statements in BOTH
// `src/db.ts::initDb()` AND `crates/augmentagent-store/src/store.rs::migrate()`.
// Order-independent; any drift on either side will fail this test.
const EXPECTED_COLUMNS = {
  actions: [
    "id",
    "messageId",
    "threadId",
    "fromEmail",
    "recipientEmail",
    "subject",
    "originalBody",
    "draftBody",
    "status",
    "errorMessage",
    "createdAt",
    "updatedAt",
  ],
  senders: ["id", "email", "label", "active", "createdAt"],
  config: ["key", "value", "updatedAt"],
  gmail_accounts: [
    "id",
    "connectionId",
    "email",
    "label",
    "entityId",
    "active",
    "createdAt",
  ],
  emails: [
    "messageId",
    "threadId",
    "fromEmail",
    "subject",
    "body",
    "receivedAt",
    "accountEntityId",
    "firstSeenAt",
    "triageResult",
    "agentProcessedAt",
    "platform",
    "kind",
  ],
  channel_subscriptions: [
    "id",
    "platform",
    "channel_id",
    "display_name",
    "mode",
    "active",
    "account_id",
    "last_seen_message_id",
    "last_digest_at_ms",
    "created_at_ms",
    "updated_at_ms",
  ],
  slack_workspaces: [
    "id",
    "team_id",
    "team_name",
    "entity_id",
    "connection_id",
    "user_id",
    "active",
    "created_at_ms",
  ],
  drive_accounts: [
    "id",
    "connection_id",
    "entity_id",
    "email",
    "label",
    "active",
    "created_at_ms",
  ],
  drive_sync_state: ["entity_id", "page_token", "updated_at_ms"],
};

test.before(() => {
  // Always start from a clean file so the assertions reflect what initDb
  // creates from scratch, not whatever migrations layered on top.
  try {
    fs.unlinkSync(tmpDb);
  } catch (_) {}
  initDb(tmpDb);
});

test.after(() => {
  try {
    fs.unlinkSync(tmpDb);
  } catch (_) {}
});

test("initDb creates every Node-owned base table with the expected columns", () => {
  const db = getDb();
  for (const [table, expected] of Object.entries(EXPECTED_COLUMNS)) {
    const rows = db.prepare(`PRAGMA table_info(${table})`).all();
    assert.ok(rows.length > 0, `table '${table}' is missing after initDb`);
    const actual = rows.map((r) => r.name).sort();
    const want = [...expected].sort();
    assert.deepStrictEqual(
      actual,
      want,
      `column drift on '${table}': Node has ${JSON.stringify(actual)}, expected ${JSON.stringify(want)}. ` +
        `If you changed the Node schema, mirror the change in crates/augmentagent-store/src/store.rs::migrate().`,
    );
  }
});
