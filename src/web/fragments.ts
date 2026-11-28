// The Jarvis Dashboard app shell and its HTML fragment routes (spec §5.3, §5.4).
// Every fragment calls the same handler functions as the JSON routes in
// routes.ts and renders HTML instead of JSON. Errors render inline with the
// HTTP status from errors.ts.

import { Router, type Request, type Response } from "express";
import type { WebThread } from "./daemonClient";
import { DaemonError } from "./errors";
import { rankPalette, type PaletteAction } from "./palette.logic";
import type { ReadDb } from "./readDb";
import { createThread, listThreads, updateThread, webReadDb } from "./routes";

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

function renderError(res: Response, err: unknown): void {
  const e = err instanceof DaemonError ? err : new DaemonError("internal");
  if (!(err instanceof DaemonError)) console.error("[web] fragment failed:", err);
  // Errors land in the toast so a failed refetch never replaces its container.
  res.status(e.status).set({ "HX-Retarget": "#j-toast", "HX-Reswap": "innerHTML" });
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

  router.get("/web/palette", handle((req, res) => {
    const threads = db().listThreads(false).map((t: WebThread) => ({ id: t.id, title: t.title, updatedAt: t.updated_at }));
    const entries = rankPalette(query(req, "q"), { actions: PALETTE_ACTIONS, threads }, { limit: 12 });
    res.render("web/palette", { entries });
  }));

  return router;
}
