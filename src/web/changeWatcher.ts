// Polls the web chat tables and tells SSE clients what changed (spec D4, §5.3).
// Cursors use `>=`, so the boundary row is re-emitted and nothing written in
// the same millisecond is missed; a duplicate only costs the browser a refetch.
// The queries are skipped while SQLite's data_version shows no new commit, so
// an idle database does not re-emit the boundary row every second.

import { EventEmitter } from "node:events";
import type { ReadDb } from "./readDb";

export const POLL_INTERVAL_MS = 1_000;

export type WatchEvent = { type: "changed"; thread_id: string } | { type: "threads_changed" };

export interface ChangeWatcherOptions {
  intervalMs?: number;
  now?: () => number;
}

export class ChangeWatcher extends EventEmitter {
  private readonly intervalMs: number;
  private turnCursor: number;
  private threadCursor: number;
  private lastVersion: number | null = null;
  private timer: NodeJS.Timeout | null = null;

  constructor(private readonly db: ReadDb, opts: ChangeWatcherOptions = {}) {
    super();
    this.intervalMs = opts.intervalMs ?? POLL_INTERVAL_MS;
    const now = (opts.now ?? Date.now)();
    this.turnCursor = now;
    this.threadCursor = now;
  }

  /** Starts polling; idempotent. Once started it runs until stop(). */
  start(): void {
    if (this.timer) return;
    this.timer = setInterval(() => this.tick(), this.intervalMs);
    this.timer.unref();
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  /** One poll. Public for tests. */
  tick(): void {
    let version: number;
    try {
      version = this.db.dataVersion();
      if (version === this.lastVersion) return;
      for (const row of this.db.turnsUpdatedSince(this.turnCursor)) {
        this.turnCursor = Math.max(this.turnCursor, row.updated_at);
        this.emit("event", { type: "changed", thread_id: row.thread_id } satisfies WatchEvent);
      }
      const changedAt = this.db.threadsChangedSince(this.threadCursor);
      if (changedAt !== null) {
        this.threadCursor = Math.max(this.threadCursor, changedAt);
        this.emit("event", { type: "threads_changed" } satisfies WatchEvent);
      }
    } catch (err) {
      // A transient read error (e.g. SQLITE_BUSY) must not stop the watcher;
      // leaving lastVersion unchanged retries on the next tick.
      console.error("[web] change watcher poll failed:", (err as Error).message);
      return;
    }
    this.lastVersion = version;
  }
}
