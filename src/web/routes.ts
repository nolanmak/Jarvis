// JSON routes and the SSE endpoint for the Jarvis Dashboard (spec D3, D4, D12, §5.3).
// Reads come from the read-only ReadDb; every write is a daemon socket op.
// The handler functions are exported so the HTML fragment routes can reuse them;
// they throw DaemonError for every failure, and errors.ts maps it to HTTP.

import { Router, type Request, type Response } from "express";
import { getActions, getDb } from "../db";
import type { ActionRecord } from "../types";
import * as daemon from "./daemonClient";
import { DaemonError } from "./errors";
import { ChangeWatcher, type WatchEvent } from "./changeWatcher";
import { MAX_MESSAGE_LIMIT, ReadDb, type ThreadLog } from "./readDb";

export const MAX_TEXT_BYTES = 32 * 1024;
export const HEARTBEAT_MS = 25_000;
export const MAX_APPROVALS = 100;
const VERBS: readonly daemon.ApprovalVerb[] = ["approve", "skip", "revise"];
// UUID shape with version nibble 7 (D12: the browser mints UUIDv7 turn ids).
const TURN_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

type Body = Record<string, unknown>;

function badRequest(message: string): DaemonError {
  return new DaemonError("bad_request", message);
}

function asBody(body: unknown): Body {
  return body && typeof body === "object" && !Array.isArray(body) ? (body as Body) : {};
}

function optionalString(body: Body, key: string): string | undefined {
  const value = body[key];
  if (value === undefined) return undefined;
  if (typeof value !== "string") throw badRequest(`${key} must be a string`);
  return value;
}

function optionalBool(body: Body, key: string): boolean | undefined {
  const value = body[key];
  if (value === undefined) return undefined;
  if (typeof value !== "boolean") throw badRequest(`${key} must be a boolean`);
  return value;
}

function optionalPositiveInt(raw: unknown, key: string): number | undefined {
  if (raw === undefined || raw === "") return undefined;
  const n = typeof raw === "string" && /^\d+$/.test(raw) ? Number(raw) : NaN;
  if (!Number.isSafeInteger(n) || n < 1) throw badRequest(`${key} must be a positive integer`);
  return n;
}

// --- handlers (shared with fragment routes) ---

export function listThreads(db: ReadDb, archived: boolean): { threads: daemon.WebThread[] } {
  return { threads: db.listThreads(archived) };
}

export function createThread(body: unknown): Promise<daemon.ThreadResult> {
  const title = optionalString(asBody(body), "title");
  return daemon.threadCreate(title === undefined ? {} : { title });
}

/** Rename and/or pin/archive; returns the thread after the last op. */
export async function updateThread(threadId: string, body: unknown): Promise<daemon.ThreadResult> {
  const b = asBody(body);
  const title = optionalString(b, "title");
  const pinned = optionalBool(b, "pinned");
  const archived = optionalBool(b, "archived");
  if (title === undefined && pinned === undefined && archived === undefined) {
    throw badRequest("Nothing to update");
  }
  let result: daemon.ThreadResult | undefined;
  if (title !== undefined) result = await daemon.threadRename({ thread_id: threadId, title });
  if (pinned !== undefined || archived !== undefined) {
    const set: { thread_id: string; pinned?: boolean; archived?: boolean } = { thread_id: threadId };
    if (pinned !== undefined) set.pinned = pinned;
    if (archived !== undefined) set.archived = archived;
    result = await daemon.threadSet(set);
  }
  return result!;
}

export function resetSession(threadId: string): Promise<daemon.ThreadResult> {
  return daemon.threadResetSession({ thread_id: threadId });
}

export function threadLog(db: ReadDb, threadId: string, query: { before?: unknown; limit?: unknown }): ThreadLog {
  const before = optionalPositiveInt(query.before, "before");
  const limit = optionalPositiveInt(query.limit, "limit");
  if (!db.getThread(threadId)) throw new DaemonError("thread_not_found", "Thread not found");
  return db.threadLog(threadId, { before, limit: limit === undefined ? undefined : Math.min(limit, MAX_MESSAGE_LIMIT) });
}

/** Validates in Node first (413 before the socket), then turn.submit. */
export function submitTurn(threadId: string, body: unknown): Promise<daemon.TurnResult> {
  const b = asBody(body);
  const turnId = b.turn_id;
  const text = b.text;
  if (typeof turnId !== "string" || !TURN_ID_RE.test(turnId)) throw badRequest("turn_id must be a UUIDv7");
  if (typeof text !== "string") throw badRequest("text must be a string");
  if (Buffer.byteLength(text, "utf8") > MAX_TEXT_BYTES) {
    throw new DaemonError("text_too_large", "Message is over 32 KiB");
  }
  if (!text.trim()) throw badRequest("Message is empty");
  return daemon.turnSubmit({ thread_id: threadId, turn_id: turnId, text });
}

export function cancelTurn(turnId: string): Promise<daemon.TurnResult> {
  return daemon.turnCancel({ turn_id: turnId });
}

export function listApprovals(): { actions: ActionRecord[] } {
  return { actions: getActions({ status: "pending", limit: MAX_APPROVALS }) };
}

export function resolveApproval(actionId: string, body: unknown): Promise<daemon.ActionResult> {
  const b = asBody(body);
  const verb = b.verb as daemon.ApprovalVerb;
  if (!VERBS.includes(verb)) throw badRequest("verb must be approve, skip or revise");
  const note = optionalString(b, "note");
  if (verb === "revise" && !note?.trim()) throw badRequest("revise needs a note");
  return daemon.actionResolve(note === undefined ? { action_id: actionId, verb } : { action_id: actionId, verb, note });
}

// --- shared context ---

let defaultReadDb: ReadDb | null = null;
let defaultWatcher: ChangeWatcher | null = null;

/** The process-wide read-only connection, opened on the DB file initDb resolved. */
export function webReadDb(): ReadDb {
  if (!defaultReadDb) defaultReadDb = new ReadDb(getDb().name);
  return defaultReadDb;
}

/** The process-wide change watcher; started on first use and never stopped. */
export function webWatcher(): ChangeWatcher {
  if (!defaultWatcher) {
    defaultWatcher = new ChangeWatcher(webReadDb());
    defaultWatcher.setMaxListeners(0);
    defaultWatcher.start();
  }
  return defaultWatcher;
}

export interface WebRoutesOptions {
  readDb?: () => ReadDb;
  watcher?: () => ChangeWatcher;
  heartbeatMs?: number;
}

function sendError(res: Response, err: unknown): void {
  if (err instanceof DaemonError) {
    res.status(err.status).json({ code: err.code, error: err.message });
    return;
  }
  console.error("[web] route failed:", err);
  res.status(500).json({ code: "internal", error: "Internal error" });
}

function json(status: number, fn: (req: Request) => unknown) {
  return async (req: Request, res: Response) => {
    try {
      res.status(status).json(await fn(req));
    } catch (err) {
      sendError(res, err);
    }
  };
}

function streamEvents(watcher: ChangeWatcher, heartbeatMs: number, req: Request, res: Response): void {
  res.status(200).set({
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache, no-transform",
    Connection: "keep-alive",
    "X-Accel-Buffering": "no",
  });
  res.flushHeaders();
  res.write(": connected\n\n");
  const onEvent = (ev: WatchEvent) => {
    const data = ev.type === "changed" ? { thread_id: ev.thread_id } : {};
    res.write(`event: ${ev.type}\ndata: ${JSON.stringify(data)}\n\n`);
  };
  const heartbeat = setInterval(() => res.write(": heartbeat\n\n"), heartbeatMs);
  watcher.on("event", onEvent);
  req.on("close", () => {
    clearInterval(heartbeat);
    watcher.off("event", onEvent);
  });
}

/** Mount behind requireAuth. */
export function createWebRoutes(opts: WebRoutesOptions = {}): Router {
  const db = opts.readDb ?? webReadDb;
  const watcher = opts.watcher ?? webWatcher;
  const heartbeatMs = opts.heartbeatMs ?? HEARTBEAT_MS;
  const id = (req: Request) => String(req.params.id);
  const router = Router();

  router.get("/api/web/threads", json(200, (req) => listThreads(db(), req.query.archived === "1")));
  router.post("/api/web/threads", json(201, (req) => createThread(req.body)));
  router.patch("/api/web/threads/:id", json(200, (req) => updateThread(id(req), req.body)));
  router.post("/api/web/threads/:id/reset-session", json(200, (req) => resetSession(id(req))));
  router.get(
    "/api/web/threads/:id/messages",
    json(200, (req) => threadLog(db(), id(req), req.query as { before?: unknown; limit?: unknown })),
  );
  router.post("/api/web/threads/:id/turns", json(202, (req) => submitTurn(id(req), req.body)));
  router.post("/api/web/turns/:id/cancel", json(200, (req) => cancelTurn(id(req))));
  router.get("/api/web/approvals", json(200, () => listApprovals()));
  router.post("/api/web/approvals/:id", json(200, (req) => resolveApproval(id(req), req.body)));
  router.get("/api/web/events", (req, res) => {
    try {
      streamEvents(watcher(), heartbeatMs, req, res);
    } catch (err) {
      sendError(res, err);
    }
  });
  return router;
}
