// #1 — split-deployment versioned JSON API + #47 cross-surface SSE.
//
// `src/dashboard.ts` serves HTMX partials for the local web UI. This module
// adds a *machine* API under `/api/v1/*` that returns plain JSON with full
// data parity, so the dashboard can run on a different host than the daemon
// (split deployment). All `/api/v1/*` routes require an API key when one is
// configured.
//
// MODE env:
//   - MODE=local  (default) — single host; API key optional.
//   - MODE=split            — UI + daemon on different hosts; API key REQUIRED
//                             (the router refuses to serve v1 without it).
//
// Exposure (split mode): put the daemon behind ngrok or a Cloudflare Tunnel
// and point the remote dashboard at it:
//   ngrok http 3000
//   cloudflared tunnel --url http://localhost:3000
// then set AUGMENTAGENT_API_BASE + AUGMENTAGENT_API_KEY on the UI host.
//
// #47: `/api/v1/events` is a Server-Sent-Events stream. Status mutations made
// through this API publish a `status` event so any connected dashboard (or a
// second surface) live-updates instead of polling. The Rust daemon's
// in-process tokio broadcast is the daemon-side analogue; this is the
// HTTP-side bus for the web surface.

import { Router, Request, Response, NextFunction } from "express";
import { EventEmitter } from "events";
import {
  getActions,
  getActionById,
  getActionCount,
  getStats,
  getSenders,
  addSender,
  removeSender,
  updateActionStatus,
} from "./db";
import type { ActionStatus } from "./types";

export const MODE = (process.env.MODE || "local").toLowerCase();
const API_KEY = process.env.AUGMENTAGENT_API_KEY || "";

// In-process bus shared with the dashboard for cross-surface sync (#47).
export const stateBus = new EventEmitter();
stateBus.setMaxListeners(64);

/** Publish a cross-surface status change. `source` lets a surface ignore its
 *  own echo (mirrors the Rust `StatusChanged{source}` broadcast). */
export function publishStatusChange(
  actionId: string,
  newStatus: string,
  source: string
): void {
  stateBus.emit("status", { actionId, newStatus, source, at: Date.now() });
}

// API-key middleware. In split mode a key is mandatory; in local mode it's
// only enforced if one is set (so existing single-host setups keep working).
function requireApiKey(req: Request, res: Response, next: NextFunction): void {
  if (MODE === "split" && !API_KEY) {
    res
      .status(503)
      .json({ error: "MODE=split requires AUGMENTAGENT_API_KEY to be set" });
    return;
  }
  if (API_KEY) {
    const provided = req.header("x-api-key");
    if (provided !== API_KEY) {
      res.status(401).json({ error: "invalid or missing x-api-key" });
      return;
    }
  }
  next();
}

const v1 = Router();
v1.use(requireApiKey);

// GET /api/v1/stats — same numbers the dashboard stats partial renders.
v1.get("/stats", (_req, res) => {
  res.json(getStats());
});

// GET /api/v1/actions — paginated list, parity with the HTMX /api/actions.
v1.get("/actions", (req, res) => {
  const status = req.query.status as ActionStatus | undefined;
  const platform = req.query.platform as string | undefined;
  const page = parseInt(req.query.page as string) || 1;
  const limit = parseInt(req.query.limit as string) || 20;
  const offset = (page - 1) * limit;
  const resolvedStatus =
    status === ("all" as any) ? undefined : status;
  const resolvedPlatform = platform === "all" ? undefined : platform;
  const actions = getActions({
    limit,
    offset,
    status: resolvedStatus,
    platform: resolvedPlatform,
  });
  const total = getActionCount(resolvedStatus, resolvedPlatform);
  res.json({
    actions,
    page,
    limit,
    total,
    totalPages: Math.ceil(total / limit),
  });
});

// GET /api/v1/actions/:id
v1.get("/actions/:id", (req, res) => {
  const action = getActionById(req.params.id);
  if (!action) {
    res.status(404).json({ error: "action not found" });
    return;
  }
  res.json(action);
});

// POST /api/v1/actions/:id — mutate status (approve/skip/etc) from a remote
// surface. Publishes a cross-surface event (#47).
v1.post("/actions/:id", (req, res) => {
  const action = getActionById(req.params.id);
  if (!action) {
    res.status(404).json({ error: "action not found" });
    return;
  }
  const { status, draftBody, errorMessage, source } = req.body || {};
  if (!status) {
    res.status(400).json({ error: "body.status is required" });
    return;
  }
  // CAS-ish guard: only mutate if still pending, mirroring the Rust
  // try_resolve_action gate so two surfaces can't double-resolve.
  if (action.status !== "pending") {
    res
      .status(409)
      .json({ error: `action already ${action.status}`, action });
    return;
  }
  updateActionStatus(req.params.id, status as ActionStatus, {
    draftBody,
    errorMessage,
  });
  publishStatusChange(req.params.id, status, source || "api_v1");
  res.json(getActionById(req.params.id));
});

// GET /api/v1/senders
v1.get("/senders", (_req, res) => {
  res.json(getSenders());
});

// POST /api/v1/senders { email, label }
v1.post("/senders", (req, res) => {
  const { email, label } = req.body || {};
  if (!email || !String(email).includes("@")) {
    res.status(400).json({ error: "valid email required" });
    return;
  }
  addSender(email, label);
  res.status(201).json(getSenders());
});

// DELETE /api/v1/senders/:id
v1.delete("/senders/:id", (req, res) => {
  removeSender(req.params.id);
  res.status(204).end();
});

// GET /api/v1/events — Server-Sent Events. Pushes `status` events so a remote
// dashboard live-updates the queue view instead of polling (#47).
v1.get("/events", (req, res) => {
  res.set({
    "Content-Type": "text/event-stream",
    "Cache-Control": "no-cache",
    Connection: "keep-alive",
  });
  res.flushHeaders?.();
  res.write(`event: hello\ndata: {"ok":true}\n\n`);

  const onStatus = (payload: unknown) => {
    res.write(`event: status\ndata: ${JSON.stringify(payload)}\n\n`);
  };
  stateBus.on("status", onStatus);

  const keepalive = setInterval(() => {
    res.write(`: keepalive\n\n`);
  }, 25000);

  req.on("close", () => {
    clearInterval(keepalive);
    stateBus.off("status", onStatus);
  });
});

const apiV1Router = Router();
apiV1Router.use("/api/v1", v1);

export default apiV1Router;
