import Database from "better-sqlite3";
import crypto from "crypto";
import path from "path";
import type {
  ActionRecord,
  ActionStatus,
  ChannelSubscription,
  DashboardStats,
  Sender,
  SubscriptionMode,
} from "./types";

let db: Database.Database;

export function initDb(dbPath?: string): Database.Database {
  // SCHEMA OWNERSHIP: As of #45, the Rust crate `augmentagent-store` is the
  // authoritative source for all SQLite schema. The CREATE TABLE IF NOT EXISTS
  // statements below are retained as defense-in-depth so the dashboard can
  // initialize a database even if the Rust daemon hasn't run yet (e.g., during
  // concurrent systemd startup). They are idempotent no-ops once Rust has
  // migrated. The additive ALTER TABLE blocks below are kept for legacy DBs
  // that pre-date the Rust ALTERs.
  const resolvedPath = dbPath || path.join(process.cwd(), "data.db");
  db = new Database(resolvedPath);

  db.pragma("journal_mode = WAL");
  db.pragma("foreign_keys = ON");

  db.exec(`
    CREATE TABLE IF NOT EXISTS actions (
      id TEXT PRIMARY KEY,
      messageId TEXT NOT NULL,
      threadId TEXT,
      fromEmail TEXT NOT NULL,
      recipientEmail TEXT,
      subject TEXT NOT NULL,
      originalBody TEXT,
      draftBody TEXT,
      status TEXT NOT NULL DEFAULT 'pending',
      errorMessage TEXT,
      createdAt INTEGER NOT NULL,
      updatedAt INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS senders (
      id TEXT PRIMARY KEY,
      email TEXT UNIQUE NOT NULL,
      label TEXT,
      active INTEGER DEFAULT 1,
      createdAt INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS config (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL,
      updatedAt INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS gmail_accounts (
      id TEXT PRIMARY KEY,
      connectionId TEXT NOT NULL,
      email TEXT,
      label TEXT,
      entityId TEXT NOT NULL,
      active INTEGER DEFAULT 1,
      createdAt INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS emails (
      messageId TEXT PRIMARY KEY,
      threadId TEXT,
      fromEmail TEXT NOT NULL,
      subject TEXT NOT NULL,
      body TEXT,
      receivedAt TEXT,
      accountEntityId TEXT,
      firstSeenAt INTEGER NOT NULL,
      triageResult TEXT,
      agentProcessedAt INTEGER,
      platform TEXT NOT NULL DEFAULT 'gmail',
      kind TEXT NOT NULL DEFAULT 'dm'
    );

    CREATE TABLE IF NOT EXISTS channel_subscriptions (
      id                   TEXT PRIMARY KEY,
      platform             TEXT NOT NULL,
      channel_id           TEXT NOT NULL,
      display_name         TEXT NOT NULL,
      mode                 TEXT NOT NULL,
      active               INTEGER NOT NULL DEFAULT 1,
      account_id           TEXT,
      last_seen_message_id TEXT,
      last_digest_at_ms    INTEGER,
      created_at_ms        INTEGER NOT NULL,
      updated_at_ms        INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS slack_workspaces (
      id              TEXT PRIMARY KEY,
      team_id         TEXT NOT NULL UNIQUE,
      team_name       TEXT NOT NULL,
      entity_id       TEXT NOT NULL,
      connection_id   TEXT NOT NULL,
      user_id         TEXT NOT NULL,
      active          INTEGER NOT NULL DEFAULT 1,
      created_at_ms   INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_actions_status ON actions(status);
    CREATE INDEX IF NOT EXISTS idx_actions_created ON actions(createdAt);
    CREATE INDEX IF NOT EXISTS idx_actions_messageId ON actions(messageId);
    CREATE INDEX IF NOT EXISTS idx_gmail_accounts_active ON gmail_accounts(active);
    CREATE INDEX IF NOT EXISTS idx_emails_triage ON emails(triageResult);
    CREATE INDEX IF NOT EXISTS idx_emails_seen ON emails(firstSeenAt);
    CREATE INDEX IF NOT EXISTS idx_emails_platform ON emails(platform);
    CREATE INDEX IF NOT EXISTS idx_channel_subs_active_mode ON channel_subscriptions(active, mode);
    CREATE INDEX IF NOT EXISTS idx_slack_workspaces_active ON slack_workspaces(active);

    -- Multi-tenant Google Drive (Composio). Schema MUST match the Rust
    -- daemon's migrate() (snake_case) since both share the tenant's data.db.
    -- Empty + unread in prod (no Drive connected) — proven-safe pattern.
    CREATE TABLE IF NOT EXISTS drive_accounts (
      id            TEXT PRIMARY KEY,
      connection_id TEXT NOT NULL,
      entity_id     TEXT NOT NULL,
      email         TEXT,
      label         TEXT,
      active        INTEGER NOT NULL DEFAULT 1,
      created_at_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_drive_accounts_active ON drive_accounts(active);
    CREATE TABLE IF NOT EXISTS drive_sync_state (
      entity_id     TEXT PRIMARY KEY,
      page_token    TEXT NOT NULL,
      updated_at_ms INTEGER NOT NULL
    );
    -- #238: SocialAPI.ai integration. Mirrors the Rust store migration so
    -- both daemons share an identical schema. socialapi_accounts is the
    -- registry of managed handles; socialapi_seen_comments dedupes inbound
    -- comments per post.
    CREATE TABLE IF NOT EXISTS socialapi_accounts (
      id             TEXT PRIMARY KEY,
      brand_id       TEXT,
      platform       TEXT NOT NULL,
      display_name   TEXT,
      account_handle TEXT,
      active         INTEGER NOT NULL DEFAULT 1,
      created_at_ms  INTEGER NOT NULL,
      updated_at_ms  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_socialapi_accounts_active ON socialapi_accounts(active);
    CREATE TABLE IF NOT EXISTS socialapi_seen_comments (
      post_id       TEXT NOT NULL,
      comment_id    TEXT NOT NULL,
      author        TEXT,
      text          TEXT,
      seen_at_ms    INTEGER NOT NULL,
      PRIMARY KEY (post_id, comment_id)
    );
  `);

  // Additive migrations for existing DBs created before the platform/kind columns landed.
  // Safe to run on every boot: column_exists guards make each ALTER idempotent, and the
  // one-shot LinkedIn fixup only touches rows still tagged 'gmail' with a LinkedIn URN.
  const emailCols = db.prepare("PRAGMA table_info(emails)").all() as { name: string }[];
  const hasPlatform = emailCols.some((c) => c.name === "platform");
  const hasKind = emailCols.some((c) => c.name === "kind");
  if (!hasPlatform) {
    db.exec("ALTER TABLE emails ADD COLUMN platform TEXT NOT NULL DEFAULT 'gmail'");
    db.exec("UPDATE emails SET platform = 'linkedin' WHERE accountEntityId LIKE 'urn:li:%'");
  }
  if (!hasKind) {
    db.exec("ALTER TABLE emails ADD COLUMN kind TEXT NOT NULL DEFAULT 'dm'");
  }

  // #36: recipientEmail lets the operator override the "To:" address on a
  // pending action card before approval — older DBs won't have the column.
  const actionCols = db.prepare("PRAGMA table_info(actions)").all() as { name: string }[];
  if (!actionCols.some((c) => c.name === "recipientEmail")) {
    db.exec("ALTER TABLE actions ADD COLUMN recipientEmail TEXT");
  }

  // Multi-workspace Slack: drop the old (platform, channel_id) UNIQUE when we
  // add account_id. SQLite can't ALTER constraints, so on old DBs that still
  // carry it we rebuild the table.
  const subCols = db.prepare("PRAGMA table_info(channel_subscriptions)").all() as { name: string }[];
  const hasAccountId = subCols.some((c) => c.name === "account_id");
  if (subCols.length > 0 && !hasAccountId) {
    db.exec(`
      BEGIN TRANSACTION;
      CREATE TABLE channel_subscriptions_new (
        id                   TEXT PRIMARY KEY,
        platform             TEXT NOT NULL,
        channel_id           TEXT NOT NULL,
        display_name         TEXT NOT NULL,
        mode                 TEXT NOT NULL,
        active               INTEGER NOT NULL DEFAULT 1,
        account_id           TEXT,
        last_seen_message_id TEXT,
        last_digest_at_ms    INTEGER,
        created_at_ms        INTEGER NOT NULL,
        updated_at_ms        INTEGER NOT NULL
      );
      INSERT INTO channel_subscriptions_new
        (id, platform, channel_id, display_name, mode, active,
         last_seen_message_id, last_digest_at_ms, created_at_ms, updated_at_ms)
        SELECT id, platform, channel_id, display_name, mode, active,
               last_seen_message_id, last_digest_at_ms, created_at_ms, updated_at_ms
          FROM channel_subscriptions;
      DROP TABLE channel_subscriptions;
      ALTER TABLE channel_subscriptions_new RENAME TO channel_subscriptions;
      CREATE INDEX IF NOT EXISTS idx_channel_subs_active_mode
        ON channel_subscriptions(active, mode);
      COMMIT;
    `);
  }

  // #240: SocialAPI.ai outbound publisher target. Mirrors the Rust store
  // migration that adds scheduled_posts.socialapi_account_id (when set, the
  // post is routed through SocialAPI.ai to that connected account; platform
  // then carries the real sub-platform). scheduled_posts is a Rust-daemon
  // table, so this is a guarded no-op on Node-only DBs — kept for schema
  // parity if a shared DB ever surfaces the table to the Node side.
  const schedTable = db
    .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='scheduled_posts'")
    .get() as { name: string } | undefined;
  if (schedTable) {
    const schedCols = db.prepare("PRAGMA table_info(scheduled_posts)").all() as { name: string }[];
    if (!schedCols.some((c) => c.name === "socialapi_account_id")) {
      db.exec("ALTER TABLE scheduled_posts ADD COLUMN socialapi_account_id TEXT");
    }
  }

  return db;
}

export function getDb(): Database.Database {
  if (!db) {
    throw new Error("Database not initialized. Call initDb() first.");
  }
  return db;
}

// --- Actions ---

export function logAction(action: Omit<ActionRecord, "id" | "createdAt" | "updatedAt">): string {
  const id = crypto.randomUUID();
  const now = Date.now();

  getDb()
    .prepare(
      `INSERT INTO actions (id, messageId, threadId, fromEmail, recipientEmail, subject, originalBody, draftBody, status, errorMessage, createdAt, updatedAt)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
    )
    .run(
      id,
      action.messageId,
      action.threadId || null,
      action.fromEmail,
      action.recipientEmail || null,
      action.subject,
      action.originalBody || null,
      action.draftBody || null,
      action.status,
      action.errorMessage || null,
      now,
      now
    );

  return id;
}

// #36: extra accepts an optional recipientEmail so the operator can edit the
// "To:" address from the approval UIs before send. Build the UPDATE dynamically
// so callers can mix-and-match fields (the old draftBody/errorMessage paths
// kept their previous semantics).
export function updateActionStatus(
  id: string,
  status: ActionStatus,
  extra?: { draftBody?: string; errorMessage?: string; recipientEmail?: string }
): void {
  const now = Date.now();
  const sets: string[] = ["status = ?", "updatedAt = ?"];
  const vals: unknown[] = [status, now];

  if (extra?.draftBody !== undefined) {
    sets.push("draftBody = ?");
    vals.push(extra.draftBody);
  }
  if (extra?.errorMessage !== undefined) {
    sets.push("errorMessage = ?");
    vals.push(extra.errorMessage);
  }
  if (extra?.recipientEmail !== undefined) {
    sets.push("recipientEmail = ?");
    vals.push(extra.recipientEmail);
  }

  getDb()
    .prepare(`UPDATE actions SET ${sets.join(", ")} WHERE id = ?`)
    .run(...vals, id);
}

// #36: standalone update for the "Change To" button on the approval card —
// keeps the action's status untouched (still pending) and only swaps the
// recipient. Returns true if a row was updated.
export function updateActionRecipient(id: string, recipientEmail: string): boolean {
  const res = getDb()
    .prepare("UPDATE actions SET recipientEmail = ?, updatedAt = ? WHERE id = ?")
    .run(recipientEmail, Date.now(), id);
  return res.changes > 0;
}

export function getActions(opts: {
  limit?: number;
  offset?: number;
  status?: ActionStatus;
  platform?: string;
}): ActionRecord[] {
  const { limit = 20, offset = 0, status, platform } = opts;

  // LEFT JOIN on emails brings platform/kind into each row. Actions always reference
  // a messageId that lives in emails, so the JOIN is effectively 1:1 but kept LEFT
  // to be defensive against any orphan rows.
  const baseSelect =
    "SELECT a.*, e.platform AS platform, e.kind AS kind FROM actions a LEFT JOIN emails e ON a.messageId = e.messageId";

  const where: string[] = [];
  const params: unknown[] = [];
  if (status) {
    where.push("a.status = ?");
    params.push(status);
  }
  if (platform) {
    where.push("e.platform = ?");
    params.push(platform);
  }
  const whereClause = where.length ? ` WHERE ${where.join(" AND ")}` : "";

  return getDb()
    .prepare(`${baseSelect}${whereClause} ORDER BY a.createdAt DESC LIMIT ? OFFSET ?`)
    .all(...params, limit, offset) as ActionRecord[];
}

export function getActionById(id: string): ActionRecord | undefined {
  return getDb()
    .prepare("SELECT * FROM actions WHERE id = ?")
    .get(id) as ActionRecord | undefined;
}

export function getActionCount(status?: ActionStatus, platform?: string): number {
  const base = "SELECT COUNT(*) as count FROM actions a LEFT JOIN emails e ON a.messageId = e.messageId";
  const where: string[] = [];
  const params: unknown[] = [];
  if (status) {
    where.push("a.status = ?");
    params.push(status);
  }
  if (platform) {
    where.push("e.platform = ?");
    params.push(platform);
  }
  const whereClause = where.length ? ` WHERE ${where.join(" AND ")}` : "";
  const row = getDb()
    .prepare(`${base}${whereClause}`)
    .get(...params) as { count: number };
  return row.count;
}

export function isMessageProcessed(messageId: string): boolean {
  const row = getDb()
    .prepare("SELECT 1 FROM actions WHERE messageId = ?")
    .get(messageId);
  return !!row;
}

export function getStats(): DashboardStats {
  const total = getActionCount();
  const approved = getActionCount("approved");
  const rejected = getActionCount("rejected");
  const sent = getActionCount("sent");
  const errored = getActionCount("error");

  const todayStart = new Date();
  todayStart.setHours(0, 0, 0, 0);
  const todayRow = getDb()
    .prepare("SELECT COUNT(*) as count FROM actions WHERE createdAt >= ?")
    .get(todayStart.getTime()) as { count: number };

  const approvalRate =
    total > 0 ? Math.round(((approved + sent) / total) * 100) : 0;

  return {
    total,
    approved,
    rejected,
    sent,
    errored,
    todayCount: todayRow.count,
    approvalRate,
  };
}

export function getRecentProcessedIds(limit = 50): string[] {
  const rows = getDb()
    .prepare("SELECT messageId FROM actions ORDER BY createdAt DESC LIMIT ?")
    .all(limit) as { messageId: string }[];
  return rows.map((r) => r.messageId);
}

// --- Senders ---

export function getSenders(): Sender[] {
  return getDb()
    .prepare("SELECT * FROM senders ORDER BY createdAt DESC")
    .all()
    .map((row: any) => ({ ...row, active: !!row.active })) as Sender[];
}

export function getActiveSenders(): string[] {
  const rows = getDb()
    .prepare("SELECT email FROM senders WHERE active = 1")
    .all() as { email: string }[];
  return rows.map((r) => r.email);
}

export function addSender(email: string, label?: string): void {
  const id = crypto.randomUUID();
  getDb()
    .prepare(
      "INSERT OR IGNORE INTO senders (id, email, label, active, createdAt) VALUES (?, ?, ?, 1, ?)"
    )
    .run(id, email.toLowerCase().trim(), label || null, Date.now());
}

export function removeSender(id: string): void {
  getDb().prepare("DELETE FROM senders WHERE id = ?").run(id);
}

export function toggleSender(id: string): void {
  getDb()
    .prepare("UPDATE senders SET active = CASE WHEN active = 1 THEN 0 ELSE 1 END WHERE id = ?")
    .run(id);
}

// --- #45 Web Push subscriptions (PWA approval surface) ---
//
// Shares the pwa_subscriptions table the Rust store's migrate() creates
// (CREATE TABLE IF NOT EXISTS; idempotent across both daemons). The Rust side
// sends the actual VAPID-signed pushes; the Node dashboard just records the
// browser's subscription when the user installs the PWA + grants permission.

export interface PushSubscription {
  endpoint: string;
  keys: { p256dh: string; auth: string };
}

export function addPushSubscription(sub: PushSubscription): void {
  getDb()
    .prepare(
      "CREATE TABLE IF NOT EXISTS pwa_subscriptions (" +
        "id TEXT PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE, " +
        "p256dh TEXT NOT NULL, auth TEXT NOT NULL, created_at_ms INTEGER NOT NULL)"
    )
    .run();
  getDb()
    .prepare(
      "INSERT INTO pwa_subscriptions (id, endpoint, p256dh, auth, created_at_ms) " +
        "VALUES (?, ?, ?, ?, ?) " +
        "ON CONFLICT(endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth"
    )
    .run(
      crypto.randomUUID(),
      sub.endpoint,
      sub.keys.p256dh,
      sub.keys.auth,
      Date.now()
    );
}

export function listPushSubscriptions(): PushSubscription[] {
  try {
    return (
      getDb()
        .prepare("SELECT endpoint, p256dh, auth FROM pwa_subscriptions")
        .all() as { endpoint: string; p256dh: string; auth: string }[]
    ).map((r) => ({
      endpoint: r.endpoint,
      keys: { p256dh: r.p256dh, auth: r.auth },
    }));
  } catch {
    return [];
  }
}

export function removePushSubscription(endpoint: string): void {
  try {
    getDb()
      .prepare("DELETE FROM pwa_subscriptions WHERE endpoint = ?")
      .run(endpoint);
  } catch {
    /* table may not exist yet — nothing to remove */
  }
}

// --- Gmail Accounts ---

export interface GmailAccount {
  id: string;
  connectionId: string;
  email: string | null;
  label: string | null;
  entityId: string;
  active: boolean;
  createdAt: number;
  // #179 — liveness signal written by the Rust daemon's gmail poll path.
  // `lastPolledAt` is ms since epoch of the most recent fetch attempt;
  // `lastPollOk` is 1 if that fetch succeeded, 0 if it failed (4xx/5xx),
  // null if the account has never been polled yet. The dashboard's
  // "connected" indicator (see `dashboard.ts`) uses both to compute a
  // *live* connected state rather than trusting the static `active` flag.
  lastPolledAt: number | null;
  lastPollOk: number | null;
}

export function getGmailAccounts(): GmailAccount[] {
  return getDb()
    .prepare("SELECT * FROM gmail_accounts ORDER BY createdAt DESC")
    .all()
    .map((row: any) => ({ ...row, active: !!row.active })) as GmailAccount[];
}

export function getActiveGmailAccounts(): GmailAccount[] {
  return getDb()
    .prepare("SELECT * FROM gmail_accounts WHERE active = 1 ORDER BY createdAt DESC")
    .all()
    .map((row: any) => ({ ...row, active: !!row.active })) as GmailAccount[];
}

export function addGmailAccount(connectionId: string, entityId: string, email?: string, label?: string): string {
  const id = crypto.randomUUID();
  getDb()
    .prepare(
      "INSERT OR REPLACE INTO gmail_accounts (id, connectionId, email, label, entityId, active, createdAt) VALUES (?, ?, ?, ?, ?, 1, ?)"
    )
    .run(id, connectionId, email || null, label || null, entityId, Date.now());
  return id;
}

export function removeGmailAccount(id: string): void {
  getDb().prepare("DELETE FROM gmail_accounts WHERE id = ?").run(id);
}

export function getGmailAccountByConnectionId(connectionId: string): GmailAccount | undefined {
  const row = getDb()
    .prepare("SELECT * FROM gmail_accounts WHERE connectionId = ?")
    .get(connectionId) as any;
  if (!row) return undefined;
  return { ...row, active: !!row.active };
}

// --- Multi-tenant Google Drive accounts (Composio) ---
// Shares the `drive_accounts` table with the Rust daemon (snake_case columns).
// `id` is deterministic (`drive-<connectionId>`) so Node and Rust dedupe the
// same connection identically.

export interface DriveAccount {
  id: string;
  connection_id: string;
  entity_id: string;
  email: string | null;
  label: string | null;
  active: boolean;
  created_at_ms: number;
}

export function getDriveAccounts(): DriveAccount[] {
  return getDb()
    .prepare("SELECT * FROM drive_accounts ORDER BY created_at_ms DESC")
    .all()
    .map((row: any) => ({ ...row, active: !!row.active })) as DriveAccount[];
}

export function getActiveDriveAccounts(): DriveAccount[] {
  return getDb()
    .prepare("SELECT * FROM drive_accounts WHERE active = 1 ORDER BY created_at_ms DESC")
    .all()
    .map((row: any) => ({ ...row, active: !!row.active })) as DriveAccount[];
}

export function addDriveAccount(
  connectionId: string,
  entityId: string,
  email?: string,
  label?: string
): string {
  const id = `drive-${connectionId}`;
  getDb()
    .prepare(
      "INSERT OR REPLACE INTO drive_accounts (id, connection_id, entity_id, email, label, active, created_at_ms) VALUES (?, ?, ?, ?, ?, 1, ?)"
    )
    .run(id, connectionId, entityId, email || null, label || null, Date.now());
  return id;
}

export function removeDriveAccount(id: string): void {
  getDb().prepare("DELETE FROM drive_accounts WHERE id = ?").run(id);
}

export function hasAnyGmailAccount(): boolean {
  const row = getDb()
    .prepare("SELECT 1 FROM gmail_accounts WHERE active = 1 LIMIT 1")
    .get();
  return !!row;
}

// --- SocialAPI.ai Accounts (#246) ---
//
// Registry of connected SocialAPI.ai handles. The table is created in the
// schema above (#238). The hosted-first setup flow (paste API key → sync)
// upserts rows here; each row can be enabled/disabled to control which
// accounts the daemon manages. The API key itself lives in the `config`
// table under `socialapi_api_key` (masked when surfaced).

export interface SocialApiAccount {
  id: string;
  brand_id: string | null;
  platform: string;
  display_name: string | null;
  account_handle: string | null;
  active: boolean;
  created_at_ms: number;
  updated_at_ms: number;
}

function rowToSocialApiAccount(row: any): SocialApiAccount {
  return { ...row, active: !!row.active } as SocialApiAccount;
}

export function getSocialApiAccounts(): SocialApiAccount[] {
  return getDb()
    .prepare("SELECT * FROM socialapi_accounts ORDER BY created_at_ms DESC")
    .all()
    .map(rowToSocialApiAccount);
}

export function getActiveSocialApiAccounts(): SocialApiAccount[] {
  return getDb()
    .prepare("SELECT * FROM socialapi_accounts WHERE active = 1 ORDER BY created_at_ms DESC")
    .all()
    .map(rowToSocialApiAccount);
}

export function upsertSocialApiAccount(
  id: string,
  platform: string,
  opts: {
    brandId?: string | null;
    displayName?: string | null;
    accountHandle?: string | null;
  } = {}
): string {
  const now = Date.now();
  getDb()
    .prepare(
      `INSERT INTO socialapi_accounts
         (id, brand_id, platform, display_name, account_handle, active, created_at_ms, updated_at_ms)
       VALUES (?, ?, ?, ?, ?, 1, ?, ?)
       ON CONFLICT(id) DO UPDATE SET
         brand_id = excluded.brand_id,
         platform = excluded.platform,
         display_name = excluded.display_name,
         account_handle = excluded.account_handle,
         updated_at_ms = excluded.updated_at_ms`
    )
    .run(
      id,
      opts.brandId ?? null,
      platform,
      opts.displayName ?? null,
      opts.accountHandle ?? null,
      now,
      now
    );
  return id;
}

export function setSocialApiAccountActive(id: string, active: boolean): void {
  getDb()
    .prepare("UPDATE socialapi_accounts SET active = ?, updated_at_ms = ? WHERE id = ?")
    .run(active ? 1 : 0, Date.now(), id);
}

export function removeSocialApiAccount(id: string): void {
  getDb().prepare("DELETE FROM socialapi_accounts WHERE id = ?").run(id);
}

// --- Slack Workspaces ---

export interface SlackWorkspace {
  id: string;
  teamId: string;
  teamName: string;
  entityId: string;
  connectionId: string;
  userId: string;
  active: boolean;
  createdAt: number;
}

function rowToWorkspace(row: any): SlackWorkspace {
  return {
    id: row.id,
    teamId: row.team_id,
    teamName: row.team_name,
    entityId: row.entity_id,
    connectionId: row.connection_id,
    userId: row.user_id,
    active: !!row.active,
    createdAt: row.created_at_ms,
  };
}

export function getSlackWorkspaces(): SlackWorkspace[] {
  return getDb()
    .prepare("SELECT * FROM slack_workspaces ORDER BY created_at_ms ASC")
    .all()
    .map(rowToWorkspace);
}

export function getActiveSlackWorkspaces(): SlackWorkspace[] {
  return getDb()
    .prepare("SELECT * FROM slack_workspaces WHERE active = 1 ORDER BY created_at_ms ASC")
    .all()
    .map(rowToWorkspace);
}

export function getSlackWorkspaceByTeam(teamId: string): SlackWorkspace | undefined {
  const row = getDb()
    .prepare("SELECT * FROM slack_workspaces WHERE team_id = ?")
    .get(teamId);
  return row ? rowToWorkspace(row) : undefined;
}

export function upsertSlackWorkspace(
  teamId: string,
  teamName: string,
  entityId: string,
  connectionId: string,
  userId: string
): SlackWorkspace {
  const existing = getSlackWorkspaceByTeam(teamId);
  const now = Date.now();
  if (existing) {
    getDb()
      .prepare(
        "UPDATE slack_workspaces SET team_name = ?, entity_id = ?, connection_id = ?, user_id = ?, active = 1 WHERE id = ?"
      )
      .run(teamName, entityId, connectionId, userId, existing.id);
    return getSlackWorkspaceByTeam(teamId)!;
  }
  const id = crypto.randomUUID();
  getDb()
    .prepare(
      `INSERT INTO slack_workspaces
         (id, team_id, team_name, entity_id, connection_id, user_id, active, created_at_ms)
       VALUES (?, ?, ?, ?, ?, ?, 1, ?)`
    )
    .run(id, teamId, teamName, entityId, connectionId, userId, now);
  return getSlackWorkspaceByTeam(teamId)!;
}

export function deactivateSlackWorkspace(teamId: string): void {
  getDb()
    .prepare("UPDATE slack_workspaces SET active = 0 WHERE team_id = ?")
    .run(teamId);
}

// --- Config ---

export function getConfig(key: string): string | null {
  const row = getDb()
    .prepare("SELECT value FROM config WHERE key = ?")
    .get(key) as { value: string } | undefined;
  return row?.value || null;
}

export function setConfig(key: string, value: string): void {
  getDb()
    .prepare(
      "INSERT INTO config (key, value, updatedAt) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET value = ?, updatedAt = ?"
    )
    .run(key, value, Date.now(), value, Date.now());
}

export function deleteConfig(key: string): void {
  getDb().prepare("DELETE FROM config WHERE key = ?").run(key);
}

// --- Invoice config (shared with the Rust daemon's `invoice_config` table) ---
//
// The Rust daemon owns this table (created + seeded in its migration). The
// dashboard unit can boot before the daemon has ever migrated, so every
// accessor first ensures the table exists with the same shape — additive and
// idempotent, never clobbering daemon-written rows. Note the column is
// `updated_at` (snake_case) here, not `updatedAt` like the `config` table.

function ensureInvoiceConfigTable(): void {
  getDb()
    .prepare(
      "CREATE TABLE IF NOT EXISTS invoice_config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)"
    )
    .run();
}

export function getInvoiceConfig(key: string): string | null {
  ensureInvoiceConfigTable();
  const row = getDb()
    .prepare("SELECT value FROM invoice_config WHERE key = ?")
    .get(key) as { value: string } | undefined;
  return row?.value ?? null;
}

export function setInvoiceConfig(key: string, value: string): void {
  ensureInvoiceConfigTable();
  getDb()
    .prepare(
      "INSERT INTO invoice_config (key, value, updated_at) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET value = ?, updated_at = ?"
    )
    .run(key, value, Date.now(), value, Date.now());
}

export interface InvoiceSettings {
  autoDraftEnabled: boolean;
  recipientEmail: string;
  nextNumber: number;
  fromEntity: string;
  lastBilledWeekEnd: string;
}

/** Bundle for the settings page. Applies the same defaults the Rust seed
 *  would, so the UI is coherent even before the daemon has migrated. */
export function getInvoiceSettings(): InvoiceSettings {
  return {
    autoDraftEnabled: getInvoiceConfig("auto_draft_enabled") === "true",
    recipientEmail: getInvoiceConfig("recipient_email") || "",
    nextNumber: parseInt(getInvoiceConfig("invoice_counter") || "35", 10) || 35,
    fromEntity: getInvoiceConfig("from_entity") || "",
    lastBilledWeekEnd: getInvoiceConfig("last_billed_week_end") || "",
  };
}

// --- Emails (ETL) ---

import type { Email } from "./types";

/**
 * Insert an email if it doesn't already exist.
 * Returns true if the email was new (inserted), false if it already existed.
 */
export function upsertEmail(email: Email, accountEntityId: string): boolean {
  const existing = getDb()
    .prepare("SELECT 1 FROM emails WHERE messageId = ?")
    .get(email.messageId);

  if (existing) return false;

  getDb()
    .prepare(
      `INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, accountEntityId, firstSeenAt, platform, kind)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
    )
    .run(
      email.messageId,
      email.threadId || null,
      email.from,
      email.subject,
      email.body || null,
      email.date || null,
      accountEntityId,
      Date.now(),
      email.platform || "gmail",
      email.kind || "dm"
    );

  return true;
}

export function getUnprocessedEmails(): (Email & { accountEntityId: string })[] {
  const rows = getDb()
    .prepare(
      "SELECT * FROM emails WHERE agentProcessedAt IS NULL ORDER BY firstSeenAt ASC"
    )
    .all() as any[];

  return rows.map((r) => ({
    messageId: r.messageId,
    threadId: r.threadId || "",
    from: r.fromEmail,
    subject: r.subject,
    body: r.body || "",
    date: r.receivedAt || "",
    accountEntityId: r.accountEntityId || "",
  }));
}

export function markEmailProcessed(messageId: string, triageResult: string): void {
  getDb()
    .prepare(
      "UPDATE emails SET triageResult = ?, agentProcessedAt = ? WHERE messageId = ?"
    )
    .run(triageResult, Date.now(), messageId);
}

export function getEmailHistory(opts: {
  limit?: number;
  offset?: number;
  triageResult?: string;
}): any[] {
  const { limit = 20, offset = 0, triageResult } = opts;

  if (triageResult) {
    return getDb()
      .prepare(
        "SELECT * FROM emails WHERE triageResult = ? ORDER BY firstSeenAt DESC LIMIT ? OFFSET ?"
      )
      .all(triageResult, limit, offset);
  }

  return getDb()
    .prepare("SELECT * FROM emails ORDER BY firstSeenAt DESC LIMIT ? OFFSET ?")
    .all(limit, offset);
}

/**
 * Purge emails older than the configured retention period.
 * Returns the number of rows deleted.
 * If retention is 0 or not set, keeps everything (indefinite).
 */
export function purgeOldEmails(): number {
  const retentionDays = getConfig("email_retention_days");
  if (!retentionDays || retentionDays === "0") return 0;

  const days = parseInt(retentionDays, 10);
  if (isNaN(days) || days <= 0) return 0;

  const cutoff = Date.now() - days * 24 * 60 * 60 * 1000;
  const result = getDb()
    .prepare("DELETE FROM emails WHERE firstSeenAt < ? AND agentProcessedAt IS NOT NULL")
    .run(cutoff);

  return result.changes;
}

export function getEmailCount(): number {
  const row = getDb()
    .prepare("SELECT COUNT(*) as count FROM emails")
    .get() as { count: number };
  return row.count;
}

// --- channel_subscriptions (issue #27) ---

export function listSubscriptions(
  platform?: string,
  activeOnly: boolean = true
): ChannelSubscription[] {
  let sql =
    "SELECT id, platform, channel_id, display_name, mode, active, account_id, last_seen_message_id, last_digest_at_ms, created_at_ms, updated_at_ms FROM channel_subscriptions";
  const where: string[] = [];
  const params: unknown[] = [];
  if (activeOnly) {
    where.push("active = 1");
  }
  if (platform) {
    where.push("platform = ?");
    params.push(platform);
  }
  if (where.length) sql += " WHERE " + where.join(" AND ");
  sql += " ORDER BY created_at_ms ASC";
  const rows = getDb().prepare(sql).all(...params) as Array<Omit<ChannelSubscription, "active"> & { active: number }>;
  return rows.map((r) => ({ ...r, active: !!r.active }));
}

export function getSubscription(id: string): ChannelSubscription | null {
  const row = getDb()
    .prepare(
      "SELECT id, platform, channel_id, display_name, mode, active, account_id, last_seen_message_id, last_digest_at_ms, created_at_ms, updated_at_ms FROM channel_subscriptions WHERE id = ?"
    )
    .get(id) as (Omit<ChannelSubscription, "active"> & { active: number }) | undefined;
  return row ? { ...row, active: !!row.active } : null;
}

export function upsertSubscription(
  platform: string,
  channelId: string,
  displayName: string,
  mode: SubscriptionMode,
  accountId: string | null = null
): ChannelSubscription {
  const now = Date.now();
  // IS is NULL-safe so the lookup matches Discord rows whose account_id is NULL.
  const existing = getDb()
    .prepare(
      "SELECT id FROM channel_subscriptions WHERE platform = ? AND channel_id = ? AND account_id IS ?"
    )
    .get(platform, channelId, accountId) as { id: string } | undefined;
  if (existing) {
    getDb()
      .prepare(
        "UPDATE channel_subscriptions SET display_name = ?, mode = ?, active = 1, updated_at_ms = ? WHERE id = ?"
      )
      .run(displayName, mode, now, existing.id);
    return getSubscription(existing.id)!;
  }
  const id = crypto.randomUUID();
  getDb()
    .prepare(
      `INSERT INTO channel_subscriptions
         (id, platform, channel_id, display_name, mode, active, account_id, created_at_ms, updated_at_ms)
       VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)`
    )
    .run(id, platform, channelId, displayName, mode, accountId, now, now);
  return getSubscription(id)!;
}

export function updateSubscriptionMode(id: string, mode: SubscriptionMode): void {
  getDb()
    .prepare("UPDATE channel_subscriptions SET mode = ?, updated_at_ms = ? WHERE id = ?")
    .run(mode, Date.now(), id);
}

export function deleteSubscription(id: string): void {
  // Soft delete — flip `active = 0` so (platform, channel_id) stays unique.
  getDb()
    .prepare("UPDATE channel_subscriptions SET active = 0, updated_at_ms = ? WHERE id = ?")
    .run(Date.now(), id);
}

// --- Proactive nudges (#57) ---
//
// The Rust daemon owns the `proactive_signals` + `proactive_user_actions`
// tables (created in its migration). The dashboard only reads signals and
// writes user actions; every accessor below first ensures the table exists
// (additive, idempotent) so the dashboard unit can boot before the daemon
// has ever migrated.

function ensureProactiveTables(): void {
  getDb().exec(`
    CREATE TABLE IF NOT EXISTS proactive_signals (
      id TEXT PRIMARY KEY, kind TEXT NOT NULL, person_slug TEXT,
      urgency TEXT NOT NULL, headline TEXT NOT NULL, detail TEXT NOT NULL,
      suggested_action_json TEXT, status TEXT NOT NULL, snooze_until_ms INTEGER,
      dedup_key TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
      dispatched_at_ms INTEGER
    );
    CREATE TABLE IF NOT EXISTS proactive_user_actions (
      id TEXT PRIMARY KEY, action TEXT NOT NULL, scope TEXT NOT NULL,
      created_at_ms INTEGER NOT NULL, expires_at_ms INTEGER
    );
  `);
}

export interface ProactiveSignalRow {
  id: string;
  kind: string;
  person_slug: string | null;
  urgency: string;
  headline: string;
  detail: string;
  suggested_action_json: string | null;
  status: string;
  snooze_until_ms: number | null;
  dedup_key: string;
  created_at_ms: number;
  dispatched_at_ms: number | null;
}

/** Active signals (not dismissed, not still-snoozed), newest first. */
export function listProactiveSignals(limit = 100): ProactiveSignalRow[] {
  ensureProactiveTables();
  const now = Date.now();
  return getDb()
    .prepare(
      `SELECT * FROM proactive_signals
        WHERE status != 'dismissed'
          AND (snooze_until_ms IS NULL OR snooze_until_ms <= ?)
        ORDER BY
          CASE urgency WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END,
          created_at_ms DESC
        LIMIT ?`
    )
    .all(now, limit) as ProactiveSignalRow[];
}

/** Signals for one person slug (includes dismissed for the person page). */
export function listProactiveSignalsForPerson(slug: string): ProactiveSignalRow[] {
  ensureProactiveTables();
  return getDb()
    .prepare(
      `SELECT * FROM proactive_signals WHERE person_slug = ?
        ORDER BY created_at_ms DESC LIMIT 100`
    )
    .all(slug) as ProactiveSignalRow[];
}

export function getProactiveSignal(id: string): ProactiveSignalRow | undefined {
  ensureProactiveTables();
  return getDb()
    .prepare("SELECT * FROM proactive_signals WHERE id = ?")
    .get(id) as ProactiveSignalRow | undefined;
}

/** Mark a signal dismissed (terminal). */
export function dismissProactiveSignal(id: string): void {
  ensureProactiveTables();
  getDb()
    .prepare("UPDATE proactive_signals SET status = 'dismissed' WHERE id = ?")
    .run(id);
}

/** Snooze a signal `days` into the future. */
export function snoozeProactiveSignal(id: string, days: number): void {
  ensureProactiveTables();
  const until = Date.now() + days * 24 * 60 * 60 * 1000;
  getDb()
    .prepare(
      "UPDATE proactive_signals SET status = 'snoozed', snooze_until_ms = ? WHERE id = ?"
    )
    .run(until, id);
}

/**
 * Record a user action into `proactive_user_actions`. The Rust runner
 * read-throughs this before dispatch. `action` ∈ snooze|dismiss|
 * mute_person|mute_rule. `expiresDays` undefined ⇒ permanent.
 */
export function recordProactiveUserAction(
  action: "snooze" | "dismiss" | "mute_person" | "mute_rule",
  scope: string,
  expiresDays?: number
): void {
  ensureProactiveTables();
  const id = crypto.randomUUID();
  const now = Date.now();
  const expires =
    expiresDays === undefined ? null : now + expiresDays * 24 * 60 * 60 * 1000;
  getDb()
    .prepare(
      `INSERT INTO proactive_user_actions
         (id, action, scope, created_at_ms, expires_at_ms)
       VALUES (?, ?, ?, ?, ?)`
    )
    .run(id, action, scope, now, expires);
}

/** Remove every row for an (action, scope) — "resume tracking" / "un-mute". */
export function clearProactiveUserAction(action: string, scope: string): void {
  ensureProactiveTables();
  getDb()
    .prepare(
      "DELETE FROM proactive_user_actions WHERE action = ? AND scope = ?"
    )
    .run(action, scope);
}

export interface ProactiveActionRow {
  id: string;
  action: string;
  scope: string;
  created_at_ms: number;
  expires_at_ms: number | null;
}

/** Active (non-expired) user actions — drives the "muted" UI state. */
export function listActiveProactiveUserActions(): ProactiveActionRow[] {
  ensureProactiveTables();
  const now = Date.now();
  return getDb()
    .prepare(
      `SELECT * FROM proactive_user_actions
        WHERE expires_at_ms IS NULL OR expires_at_ms > ?
        ORDER BY created_at_ms DESC`
    )
    .all(now) as ProactiveActionRow[];
}

// --- #117 multi-repo agent-coding allowlist + audit ---
//
// The Rust daemon's `migrate()` owns the canonical `agent_repos` /
// `agent_pr_runs` schema. Schema here MUST stay byte-identical (snake_case,
// same column types/defaults) since both processes share the tenant's
// data.db. Like the proactive tables, every accessor first ensures the
// tables exist (additive, idempotent) so the dashboard unit can boot before
// the daemon has ever migrated. Default-deny: an empty `agent_repos` means
// the agent can touch nothing.

function ensureAgentRepoTables(): void {
  getDb().exec(`
    CREATE TABLE IF NOT EXISTS agent_repos (
      id                 TEXT PRIMARY KEY,
      full_name          TEXT NOT NULL UNIQUE COLLATE NOCASE,
      base_branch        TEXT NOT NULL DEFAULT 'main',
      build_cmd          TEXT NOT NULL DEFAULT '',
      blast_radius_extra TEXT NOT NULL DEFAULT '',
      max_diff_lines     INTEGER NOT NULL DEFAULT 600,
      enabled            INTEGER NOT NULL DEFAULT 1,
      created_at_ms      INTEGER NOT NULL,
      updated_at_ms      INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_agent_repos_enabled ON agent_repos(enabled);
    CREATE TABLE IF NOT EXISTS agent_pr_runs (
      id             TEXT PRIMARY KEY,
      repo_full_name TEXT NOT NULL,
      issue_number   INTEGER NOT NULL,
      branch         TEXT NOT NULL,
      summary        TEXT NOT NULL DEFAULT '',
      diff_lines     INTEGER NOT NULL DEFAULT 0,
      status         TEXT NOT NULL,
      pr_url         TEXT,
      error          TEXT,
      created_at_ms  INTEGER NOT NULL,
      updated_at_ms  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_agent_pr_runs_repo
      ON agent_pr_runs(repo_full_name, created_at_ms);
    CREATE INDEX IF NOT EXISTS idx_agent_pr_runs_status
      ON agent_pr_runs(status);
  `);
}

export interface AgentRepoRow {
  id: string;
  full_name: string;
  base_branch: string;
  build_cmd: string;
  blast_radius_extra: string;
  max_diff_lines: number;
  enabled: number;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface AgentPrRunRow {
  id: string;
  repo_full_name: string;
  issue_number: number;
  branch: string;
  summary: string;
  diff_lines: number;
  status: string;
  pr_url: string | null;
  error: string | null;
  created_at_ms: number;
  updated_at_ms: number;
}

/** Allowlisted repos. `enabledOnly` filters to active grants. */
export function listAgentRepos(enabledOnly = false): AgentRepoRow[] {
  ensureAgentRepoTables();
  const sql = enabledOnly
    ? "SELECT * FROM agent_repos WHERE enabled = 1 ORDER BY full_name ASC"
    : "SELECT * FROM agent_repos ORDER BY full_name ASC";
  return getDb().prepare(sql).all() as AgentRepoRow[];
}

/** Allowlist (or re-grant / update) a repo. Idempotent on full_name. */
export function upsertAgentRepo(
  fullName: string,
  baseBranch: string,
  buildCmd: string,
  blastRadiusExtra: string,
  maxDiffLines: number
): AgentRepoRow {
  ensureAgentRepoTables();
  const now = Date.now();
  const existing = getDb()
    .prepare("SELECT id FROM agent_repos WHERE full_name = ? COLLATE NOCASE")
    .get(fullName) as { id: string } | undefined;
  if (existing) {
    getDb()
      .prepare(
        `UPDATE agent_repos SET base_branch = ?, build_cmd = ?,
                blast_radius_extra = ?, max_diff_lines = ?, enabled = 1,
                updated_at_ms = ? WHERE id = ?`
      )
      .run(baseBranch, buildCmd, blastRadiusExtra, maxDiffLines, now, existing.id);
  } else {
    getDb()
      .prepare(
        `INSERT INTO agent_repos
           (id, full_name, base_branch, build_cmd, blast_radius_extra,
            max_diff_lines, enabled, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)`
      )
      .run(
        crypto.randomUUID(),
        fullName,
        baseBranch,
        buildCmd,
        blastRadiusExtra,
        maxDiffLines,
        now,
        now
      );
  }
  return getDb()
    .prepare("SELECT * FROM agent_repos WHERE full_name = ? COLLATE NOCASE")
    .get(fullName) as AgentRepoRow;
}

/**
 * Revoke a repo: soft-disable + auto-reject its in-flight `pending_approval`
 * gate rows so a revoked repo can never get a PR opened from a stale card.
 * Returns the number of gate rows cancelled. Mirrors the Rust
 * `revoke_agent_repo` exactly.
 */
export function revokeAgentRepo(fullName: string): number {
  ensureAgentRepoTables();
  const now = Date.now();
  getDb()
    .prepare(
      "UPDATE agent_repos SET enabled = 0, updated_at_ms = ? WHERE full_name = ? COLLATE NOCASE"
    )
    .run(now, fullName);
  const res = getDb()
    .prepare(
      `UPDATE agent_pr_runs
          SET status = 'rejected', error = 'repo access revoked', updated_at_ms = ?
        WHERE repo_full_name = ? COLLATE NOCASE AND status = 'pending_approval'`
    )
    .run(now, fullName);
  return res.changes;
}

/** Per-repo (or global) agent-PR run history, newest first. */
export function listAgentPrRuns(
  fullName?: string,
  limit = 50
): AgentPrRunRow[] {
  ensureAgentRepoTables();
  if (fullName) {
    return getDb()
      .prepare(
        `SELECT * FROM agent_pr_runs WHERE repo_full_name = ? COLLATE NOCASE
          ORDER BY created_at_ms DESC LIMIT ?`
      )
      .all(fullName, limit) as AgentPrRunRow[];
  }
  return getDb()
    .prepare(
      "SELECT * FROM agent_pr_runs ORDER BY created_at_ms DESC LIMIT ?"
    )
    .all(limit) as AgentPrRunRow[];
}

/**
 * Resolve a queued gate row. CAS-guarded: only mutates when still
 * `pending_approval`, returning the updated row, or null if it wasn't
 * pending (already resolved / unknown) — mirrors the Rust transition_gate
 * so two surfaces can't double-fire.
 */
export function resolveAgentPrRun(
  id: string,
  decision: "approved" | "rejected"
): AgentPrRunRow | null {
  ensureAgentRepoTables();
  const now = Date.now();
  const res = getDb()
    .prepare(
      `UPDATE agent_pr_runs
          SET status = ?,
              error = CASE WHEN ? = 'rejected' THEN 'rejected by reviewer' ELSE error END,
              updated_at_ms = ?
        WHERE id = ? AND status = 'pending_approval'`
    )
    .run(decision, decision, now, id);
  if (res.changes === 0) return null;
  return getDb()
    .prepare("SELECT * FROM agent_pr_runs WHERE id = ?")
    .get(id) as AgentPrRunRow;
}
