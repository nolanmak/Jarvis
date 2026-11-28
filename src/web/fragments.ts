// The Jarvis Dashboard app shell and its HTML fragment routes (spec §5.3, §5.4).
// Every fragment calls the same handler functions as the JSON routes in
// routes.ts and renders HTML instead of JSON. Errors render inline with the
// HTTP status from errors.ts.

import { Router, type Request, type Response } from "express";
import type { WebThread } from "./daemonClient";
import { DaemonError } from "./errors";
import { renderPlainText } from "./markdown.logic";
import { rankPalette, type PaletteAction } from "./palette.logic";
import type { ReadDb } from "./readDb";
import { clockTime, logRows } from "./logView";
import {
  cancelTurn,
  createThread,
  listApprovals,
  listThreads,
  resolveApproval,
  resetSession,
  submitTurn,
  threadLog,
  updateThread,
  webReadDb,
} from "./routes";

/** Legacy pages, kept at their URLs and reached from "More" and the palette. */
export const LEGACY_PAGES: readonly PaletteAction[] = [
  { id: "dashboard", label: "Dashboard", href: "/dashboard" },
  { id: "queue", label: "Queue", href: "/queue" },
  { id: "history", label: "History", href: "/history" },
  { id: "subscriptions", label: "Subscriptions", href: "/subscriptions" },
  { id: "relationships", label: "Relationships", href: "/relationships" },
  { id: "repos", label: "Repos", href: "/repos" },
  { id: "sessions", label: "Sessions", href: "/sessions" },
  { id: "audit", label: "Activity", href: "/audit" },
  { id: "resume", label: "Resume", href: "/resume" },
  { id: "settings", label: "Settings", href: "/settings" },
];

const PALETTE_ACTIONS: readonly PaletteAction[] = [
  { id: "new-thread", label: "New thread", href: "/" },
  { id: "approvals", label: "Open approvals", href: "/?pane=approvals" },
  ...LEGACY_PAGES,
];

const HUMAN_ERRORS: Readonly<Record<string, string>> = {
  thread_not_found: "Thread not found.",
  turn_not_found: "That message no longer exists.",
  action_not_found: "That action no longer exists.",
  thread_archived: "This thread is archived. Unarchive it to send.",
  turn_conflict: "That message was already sent with different text.",
  already_resolved: "Already resolved elsewhere.",
  text_too_large: "Message is over 32 KiB.",
  thread_busy: "Jarvis is still working on this thread. Try again in a moment.",
  jarvis_offline: "Jarvis is offline right now.",
  web_disabled: "Jarvis web chat is not enabled on this server.",
  jarvis_timeout: "Jarvis took too long to respond.",
};

/** The owner-facing line for a daemon error code. */
export function humanError(err: DaemonError): string {
  if (err.code === "bad_request") return err.message;
  return HUMAN_ERRORS[err.code] ?? "Something went wrong.";
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/** Compact last-activity label for the sidebar: now, 5m, 3h, Wed, Sep 3, Jan 9, 2025. */
export function shortTime(ms: number, now: number = Date.now()): string {
  const age = now - ms;
  if (age < 60_000) return "now";
  if (age < 3_600_000) return `${Math.floor(age / 60_000)}m`;
  if (age < 86_400_000) return `${Math.floor(age / 3_600_000)}h`;
  const d = new Date(ms);
  if (age < 6 * 86_400_000) return DAYS[d.getDay()];
  const day = `${MONTHS[d.getMonth()]} ${d.getDate()}`;
  return d.getFullYear() === new Date(now).getFullYear() ? day : `${day}, ${d.getFullYear()}`;
}

const PLATFORMS: Readonly<Record<string, string>> = {
  gmail: "Gmail", email: "Email", discord: "Discord", slack: "Slack", imessage: "iMessage", sms: "SMS",
  whatsapp: "WhatsApp", telegram: "Telegram", linkedin: "LinkedIn", instagram: "Instagram", twitter: "X",
  reddit: "Reddit", gcal: "Calendar", calendar: "Calendar", calendly: "Calendly", luma: "Luma",
  partiful: "Partiful", meetup: "Meetup", notion: "Notion", linear: "Linear", github: "GitHub",
};

/** Product name for an action's platform id; unknown ids are capitalized, never shown raw. */
export function platformLabel(p?: string): string {
  const id = p || "gmail";
  return PLATFORMS[id] ?? id.charAt(0).toUpperCase() + id.slice(1);
}
// Schemes a manual-send link may use; anything else (javascript:, data:) is dropped.
const SAFE_LINK = /^(https?:|mailto:|sms:|tel:)/i;

function approvalsLocals() {
  const { actions } = listApprovals();
  return { actions, shortTime, now: Date.now(), renderPlainText, platformLabel };
}

/** htmx form posts send strings; the shared handlers want JSON booleans. */
function formBool(value: unknown): boolean | undefined {
  if (value === undefined) return undefined;
  if (value === "true" || value === true) return true;
  if (value === "false" || value === false) return false;
  throw new DaemonError("bad_request", "Expected true or false");
}

function sidebarLocals(db: ReadDb, archived: boolean, active: string) {
  const { threads } = listThreads(db, archived);
  return { threads, archived, active, shortTime, now: Date.now() };
}

function logLocals(db: ReadDb, threadId: string) {
  const thread = db.getThread(threadId);
  return { thread, rows: logRows(threadLog(db, threadId, {})), clockTime };
}

/**
 * Errors land in the toast (or the composer) so a failed refetch never
 * replaces its container. X-Jarvis-Error carries the code for web.js (D12).
 */
function renderError(res: Response, err: unknown, target = "#j-toast"): void {
  const e = err instanceof DaemonError ? err : new DaemonError("internal");
  if (!(err instanceof DaemonError)) console.error("[web] fragment failed:", err);
  res.status(e.status).set({ "HX-Retarget": target, "HX-Reswap": "innerHTML", "X-Jarvis-Error": e.code });
  res.render("web/error", { message: humanError(e) });
}

export interface FragmentRoutesOptions {
  readDb?: () => ReadDb;
}

/** `GET /` plus the `/web/*` fragments. Mount behind requireAuth. */
export function createFragmentRoutes(opts: FragmentRoutesOptions = {}): Router {
  const db = opts.readDb ?? webReadDb;
  const router = Router();
  const handle = (fn: (req: Request, res: Response) => unknown) => async (req: Request, res: Response) => {
    try {
      await fn(req, res);
    } catch (err) {
      renderError(res, err);
    }
  };
  const query = (req: Request, key: string) => (typeof req.query[key] === "string" ? (req.query[key] as string) : "");

  router.get("/", handle((req, res) => {
    const requested = query(req, "thread");
    const thread = requested ? db().getThread(requested) : null;
    const pane = query(req, "pane") === "approvals" ? "approvals" : requested ? "chat" : "threads";
    res.render("web/app", {
      ...sidebarLocals(db(), false, thread?.id ?? ""),
      ...(thread ? logLocals(db(), thread.id) : { rows: [], clockTime }),
      ...approvalsLocals(),
      thread,
      missing: Boolean(requested && !thread),
      pane,
      legacyPages: LEGACY_PAGES,
      kbUrl: res.locals.kbUrl ?? "",
    });
  }));

  router.get("/web/threads", handle((req, res) => {
    res.render("web/threads", sidebarLocals(db(), query(req, "archived") === "1", query(req, "active")));
  }));

  router.post("/web/threads", handle(async (_req, res) => {
    const { thread } = await createThread({});
    res.set("HX-Push-Url", `/?thread=${encodeURIComponent(thread.id)}`);
    res.render("web/threads", sidebarLocals(db(), false, thread.id));
  }));

  const mutateThread = (body: (req: Request) => object) =>
    handle(async (req, res) => {
      const { thread } = await updateThread(String(req.params.id), body(req));
      res.render("web/threads", { ...sidebarLocals(db(), false, thread.id), oobThread: thread });
    });
  router.post("/web/threads/:id/rename", mutateThread((req) => ({ title: String(req.body?.title ?? "") })));
  router.post("/web/threads/:id/set", mutateThread((req) => ({
    pinned: formBool(req.body?.pinned),
    archived: formBool(req.body?.archived),
  })));

  router.get("/web/threads/:id/log", handle((req, res) => {
    res.render("web/log", logLocals(db(), String(req.params.id)));
  }));

  router.post("/web/threads/:id/turns", async (req, res) => {
    const threadId = String(req.params.id);
    try {
      await submitTurn(threadId, req.body);
    } catch (err) {
      renderError(res, err, "#composer-error");
      return;
    }
    handle((_req, r) => r.render("web/log", logLocals(db(), threadId)))(req, res);
  });

  router.post("/web/turns/:id/cancel", handle(async (req, res) => {
    await cancelTurn(String(req.params.id));
    res.render("web/log", logLocals(db(), String(req.body?.thread_id ?? "")));
  }));

  router.post("/web/threads/:id/reset-session", handle(async (req, res) => {
    const { thread } = await resetSession(String(req.params.id));
    res.render("web/log", logLocals(db(), thread.id));
  }));

  router.get("/web/approvals", handle((_req, res) => {
    res.render("web/approvals", { ...approvalsLocals(), oob: true });
  }));

  router.post("/web/approvals/:id", handle(async (req, res) => {
    // An empty revise note box rides along with every verb; drop it.
    const note = typeof req.body?.note === "string" && req.body.note.trim() ? req.body.note : undefined;
    const { outcome } = await resolveApproval(String(req.params.id), { verb: req.body?.verb, note });
    const link = outcome.link && SAFE_LINK.test(outcome.link) ? outcome.link : undefined;
    res.render("web/approvals", { ...approvalsLocals(), oob: true, outcome: { ...outcome, link } });
  }));

  router.get("/web/palette", handle((req, res) => {
    const threads = db().listThreads(false).map((t: WebThread) => ({ id: t.id, title: t.title, updatedAt: t.updated_at }));
    const entries = rankPalette(query(req, "q"), { actions: PALETTE_ACTIONS, threads }, { limit: 12 });
    res.render("web/palette", { entries });
  }));

  return router;
}
