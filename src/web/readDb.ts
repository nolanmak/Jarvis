// Read-only view of the daemon-owned web chat tables (spec D3, §5.1, §5.3).
// Node never writes these tables; every write is a daemon socket op. A missing
// web_* table reads as empty, because the dashboard may start before the
// daemon has migrated.

import Database from "better-sqlite3";
import type { TurnStatus, WebThread } from "./daemonClient";

export const DEFAULT_MESSAGE_LIMIT = 100;
export const MAX_MESSAGE_LIMIT = 200;

export interface WebMessage {
  seq: number;
  turn_id: string;
  role: "owner" | "jarvis";
  text: string;
  created_at: number;
}

export interface WebTurn {
  turn_id: string;
  status: TurnStatus;
  error_code: string | null;
  error: string | null;
  created_at: number;
  finished_at: number | null;
}

export interface ThreadLog {
  messages: WebMessage[];
  turns: WebTurn[];
}

interface ThreadRow extends Omit<WebThread, "pinned" | "archived"> {
  pinned: number;
  archived: number;
}

const THREAD_COLUMNS = "id, title, created_at, updated_at, changed_at, pinned, archived";
const TURN_COLUMNS = "turn_id, status, error_code, error, created_at, finished_at";

function toThread(row: ThreadRow): WebThread {
  return { ...row, pinned: row.pinned !== 0, archived: row.archived !== 0 };
}

function isMissingTable(err: unknown): boolean {
  return err instanceof Error && /no such table: web_/.test(err.message);
}

export class ReadDb {
  private readonly db: Database.Database;

  constructor(dbPath: string) {
    this.db = new Database(dbPath, { readonly: true, fileMustExist: true });
  }

  close(): void {
    this.db.close();
  }

  /** Runs a read; a missing web_* table yields `empty`. */
  private read<T>(empty: T, fn: () => T): T {
    try {
      return fn();
    } catch (err) {
      if (isMissingTable(err)) return empty;
      throw err;
    }
  }

  /** Increments whenever another connection commits to the database file. */
  dataVersion(): number {
    return this.db.pragma("data_version", { simple: true }) as number;
  }

  listThreads(archived: boolean): WebThread[] {
    return this.read([], () =>
      (this.db
        .prepare(`SELECT ${THREAD_COLUMNS} FROM web_threads WHERE archived = ? ORDER BY pinned DESC, updated_at DESC`)
        .all(archived ? 1 : 0) as ThreadRow[]).map(toThread),
    );
  }

  getThread(id: string): WebThread | null {
    return this.read(null, () => {
      const row = this.db.prepare(`SELECT ${THREAD_COLUMNS} FROM web_threads WHERE id = ?`).get(id) as ThreadRow | undefined;
      return row ? toThread(row) : null;
    });
  }

  /**
   * The newest `limit` messages with seq < `before` (oldest first), plus every
   * turn they reference and every non-terminal turn of the thread.
   */
  threadLog(threadId: string, opts: { before?: number; limit?: number } = {}): ThreadLog {
    const limit = Math.min(opts.limit ?? DEFAULT_MESSAGE_LIMIT, MAX_MESSAGE_LIMIT);
    return this.read({ messages: [], turns: [] }, () => {
      const messages = (this.db
        .prepare(
          `SELECT seq, turn_id, role, text, created_at FROM web_chat_messages
           WHERE thread_id = ? AND seq < ? ORDER BY seq DESC LIMIT ?`,
        )
        .all(threadId, opts.before ?? Number.MAX_SAFE_INTEGER, limit) as WebMessage[]).reverse();
      const turnIds = [...new Set(messages.map((m) => m.turn_id))];
      const turns = this.db
        .prepare(
          `SELECT ${TURN_COLUMNS} FROM web_turns
           WHERE thread_id = ? AND (status IN ('queued','running') OR turn_id IN (SELECT value FROM json_each(?)))
           ORDER BY created_at, turn_id`,
        )
        .all(threadId, JSON.stringify(turnIds)) as WebTurn[];
      return { messages, turns };
    });
  }

  /** Watcher query: latest web_turns.updated_at per thread, at or after `cursor`. */
  turnsUpdatedSince(cursor: number): Array<{ thread_id: string; updated_at: number }> {
    return this.read([], () =>
      this.db
        .prepare(
          `SELECT thread_id, max(updated_at) AS updated_at FROM web_turns
           WHERE updated_at >= ? GROUP BY thread_id`,
        )
        .all(cursor) as Array<{ thread_id: string; updated_at: number }>,
    );
  }

  /** Watcher query: latest web_threads.changed_at at or after `cursor`, or null. */
  threadsChangedSince(cursor: number): number | null {
    return this.read(null, () => {
      const row = this.db
        .prepare("SELECT max(changed_at) AS changed_at FROM web_threads WHERE changed_at >= ?")
        .get(cursor) as { changed_at: number | null };
      return row.changed_at;
    });
  }
}
