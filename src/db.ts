import Database from "better-sqlite3";
import crypto from "crypto";
import path from "path";
import type { ActionRecord, ActionStatus, DashboardStats, Sender } from "./types";

let db: Database.Database;

export function initDb(dbPath?: string): Database.Database {
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

    CREATE INDEX IF NOT EXISTS idx_actions_status ON actions(status);
    CREATE INDEX IF NOT EXISTS idx_actions_created ON actions(createdAt);
    CREATE INDEX IF NOT EXISTS idx_actions_messageId ON actions(messageId);
  `);

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
      `INSERT INTO actions (id, messageId, threadId, fromEmail, subject, originalBody, draftBody, status, errorMessage, createdAt, updatedAt)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
    )
    .run(
      id,
      action.messageId,
      action.threadId || null,
      action.fromEmail,
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

export function updateActionStatus(
  id: string,
  status: ActionStatus,
  extra?: { draftBody?: string; errorMessage?: string }
): void {
  const now = Date.now();

  if (extra?.draftBody) {
    getDb()
      .prepare("UPDATE actions SET status = ?, draftBody = ?, updatedAt = ? WHERE id = ?")
      .run(status, extra.draftBody, now, id);
  } else if (extra?.errorMessage) {
    getDb()
      .prepare("UPDATE actions SET status = ?, errorMessage = ?, updatedAt = ? WHERE id = ?")
      .run(status, extra.errorMessage, now, id);
  } else {
    getDb()
      .prepare("UPDATE actions SET status = ?, updatedAt = ? WHERE id = ?")
      .run(status, now, id);
  }
}

export function getActions(opts: {
  limit?: number;
  offset?: number;
  status?: ActionStatus;
}): ActionRecord[] {
  const { limit = 20, offset = 0, status } = opts;

  if (status) {
    return getDb()
      .prepare(
        "SELECT * FROM actions WHERE status = ? ORDER BY createdAt DESC LIMIT ? OFFSET ?"
      )
      .all(status, limit, offset) as ActionRecord[];
  }

  return getDb()
    .prepare("SELECT * FROM actions ORDER BY createdAt DESC LIMIT ? OFFSET ?")
    .all(limit, offset) as ActionRecord[];
}

export function getActionById(id: string): ActionRecord | undefined {
  return getDb()
    .prepare("SELECT * FROM actions WHERE id = ?")
    .get(id) as ActionRecord | undefined;
}

export function getActionCount(status?: ActionStatus): number {
  if (status) {
    const row = getDb()
      .prepare("SELECT COUNT(*) as count FROM actions WHERE status = ?")
      .get(status) as { count: number };
    return row.count;
  }
  const row = getDb()
    .prepare("SELECT COUNT(*) as count FROM actions")
    .get() as { count: number };
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
