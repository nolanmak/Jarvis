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

import { Router, Request, Response } from "express";
import { EventEmitter } from "events";
import { execFile } from "child_process";
import path from "path";
import fs from "fs";
import { requireAuth, newRedditState, consumeRedditState } from "./security";
import {
  getActions,
  getActionById,
  getActionCount,
  getStats,
  getSenders,
  addSender,
  removeSender,
  updateActionStatus,
  addPushSubscription,
  removePushSubscription,
  getActiveGmailAccounts,
  getActiveDriveAccounts,
  getActiveSlackWorkspaces,
  getActiveSocialApiAccounts,
  getConfig,
} from "./db";
import type { ActionStatus } from "./types";
import { isPlausibleEmail } from "./types";

export const MODE = (process.env.MODE || "local").toLowerCase();

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

// #297: auth is now ALWAYS enforced (fail-closed). `requireAuth` resolves a
// key from AUGMENTAGENT_API_KEY or a persisted/auto-generated key, and accepts
// a Bearer/x-api-key header (machine clients) OR a signed session cookie
// (browser UI). Re-exported under the historical `requireApiKey` name so the
// dashboard router and the #117 /repos admin surface share one credential and
// one middleware without further edits.
export const requireApiKey = requireAuth;

// Process start timestamp — used by `/api/v1/health` so we report uptime
// against THIS process, not the system clock. Captured at module-load
// rather than via `process.uptime()` so the CLI integration test in #10
// can reason about a stable monotonic source.
const PROCESS_STARTED_MS = Date.now();

// Cached `package.json#version` for the health probe. The dashboard runs
// from `dist/`, so resolve relative to the source root via `__dirname`.
// Falls back to "unknown" if the file can't be read (defensive — the
// preflight check just needs *something* non-empty in the version field).
function readPackageVersion(): string {
  const candidates = [
    path.join(__dirname, "..", "package.json"),
    path.join(process.cwd(), "package.json"),
  ];
  for (const p of candidates) {
    try {
      const raw = fs.readFileSync(p, "utf8");
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed.version === "string") {
        return parsed.version;
      }
    } catch {
      // try the next candidate
    }
  }
  return "unknown";
}
const PACKAGE_VERSION = readPackageVersion();

// Public preflight surface for the `augmentagent setup oauth` orchestrator
// (#10). NOT gated by `requireApiKey` — the CLI uses this to decide whether
// the dashboard is up *before* it has any creds in hand, and there's no
// sensitive data in the response.
const publicV1 = Router();
publicV1.get("/health", (_req, res) => {
  res.json({
    ok: true,
    version: PACKAGE_VERSION,
    uptime_secs: Math.floor((Date.now() - PROCESS_STARTED_MS) / 1000),
  });
});

const v1 = Router();
v1.use(requireAuth);

// GET /api/v1/stats — same numbers the dashboard stats partial renders.
v1.get("/stats", (_req, res) => {
  res.json(getStats());
});

// GET /api/v1/oauth/status — rollup of per-provider connection state used
// by the `augmentagent setup oauth` orchestrator (#10). Composes the same
// helpers backing the individual `/api/oauth/<provider>/status` routes in
// `dashboard.ts` so a "new connection appeared" diff between two snapshots
// is the canonical success signal. Reddit has no accounts table — its
// keychain refresh token is the proof-of-life, so `getConfig` is enough.
v1.get("/oauth/status", (_req, res) => {
  const gmailAccounts = getActiveGmailAccounts().map((a) => ({
    id: a.id,
    email: a.email,
    entityId: a.entityId,
  }));
  const driveAccounts = getActiveDriveAccounts().map((a) => ({
    id: a.id,
    email: a.email,
    entityId: a.entity_id,
  }));
  const slackWorkspaces = getActiveSlackWorkspaces().map((w) => ({
    id: w.id,
    team_id: w.teamId,
    team_name: w.teamName,
    user_id: w.userId,
  }));
  const redditConnected = !!getConfig("reddit_refresh_token");
  const socialApiAccounts = getActiveSocialApiAccounts().map((a) => ({
    id: a.id,
    platform: a.platform,
    display_name: a.display_name,
    account_handle: a.account_handle,
  }));
  res.json({
    gmail: { accounts: gmailAccounts, lastError: null },
    googledrive: { accounts: driveAccounts, lastError: null },
    slack: { workspaces: slackWorkspaces, lastError: null },
    reddit: { connected: redditConnected },
    socialapi: { accounts: socialApiAccounts, lastError: null },
  });
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
//
// #36: also accepts `recipientEmail` so the dashboard / PWA / Discord webhook
// can swap the "To:" address before approval. Validated against a minimal
// shape check (`isPlausibleEmail`) — we don't pretend to do MX lookups.
v1.post("/actions/:id", (req, res) => {
  const action = getActionById(req.params.id);
  if (!action) {
    res.status(404).json({ error: "action not found" });
    return;
  }
  const { status, draftBody, errorMessage, recipientEmail, source } = req.body || {};
  if (!status) {
    res.status(400).json({ error: "body.status is required" });
    return;
  }
  if (recipientEmail !== undefined && !isPlausibleEmail(recipientEmail)) {
    res.status(400).json({ error: "recipientEmail is not a valid email address" });
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
    recipientEmail,
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
// Mount the public preflight surface first so `/api/v1/health` short-circuits
// before Express ever consults the API-key middleware on the gated `v1` chain.
apiV1Router.use("/api/v1", publicV1);
apiV1Router.use("/api/v1", v1);

// --- #45 PWA approval surface: queue route + Web Push subscription ---

const VAPID_PUBLIC = process.env.VAPID_PUBLIC_KEY || "";

const pwa = Router();

// Installable PWA queue view. Deep-linkable via ?action=<id> (the service
// worker focuses here on notification click).
pwa.get("/queue", (_req, res) => {
  res.render("queue", { vapidPublic: VAPID_PUBLIC });
});

// The browser fetches the VAPID public key to subscribe.
pwa.get("/api/push/vapid", (_req, res) => {
  res.json({ publicKey: VAPID_PUBLIC });
});

pwa.post("/api/push/subscribe", (req, res) => {
  const sub = req.body;
  if (!sub || !sub.endpoint || !sub.keys || !sub.keys.p256dh || !sub.keys.auth) {
    res.status(400).json({ error: "invalid subscription" });
    return;
  }
  addPushSubscription(sub);
  res.status(201).json({ ok: true });
});

pwa.post("/api/push/unsubscribe", (req, res) => {
  const endpoint = req.body && req.body.endpoint;
  if (!endpoint) {
    res.status(400).json({ error: "endpoint required" });
    return;
  }
  removePushSubscription(endpoint);
  res.json({ ok: true });
});

apiV1Router.use(pwa);


// --- #48 Reddit OAuth bootstrap (dashboard callback) ---
//
// Not under /api/v1 and NOT API-key gated — it's a browser redirect flow the
// user drives from the dashboard. Shells out to the Rust CLI so the permanent
// refresh token lands in the keyring (single source of credential truth).

const REDDIT_CLIENT_ID = process.env.REDDIT_CLIENT_ID || "";
const REDDIT_REDIRECT =
  process.env.REDDIT_REDIRECT_URI ||
  `http://localhost:${process.env.DASHBOARD_PORT || 3000}/api/reddit/callback`;

function cliPath(): string {
  return (
    process.env.AUGMENTAGENT_BIN ||
    path.join(process.cwd(), "target", "release", "augmentagent")
  );
}

const reddit = Router();

// Handlers are pulled out as named consts so we can mount them at BOTH the
// historical `/api/reddit/*` paths AND the canonical `/oauth/reddit/*` aliases
// (issue #34). The `/oauth/<slug>/start` + `/oauth/<slug>/callback` shape is
// what every other provider uses, so the CLI orchestrator and the `/setup`
// skill can stop special-casing Reddit. The originals stay live for backward
// compat — anything pinned to the old paths (registered Reddit app redirect
// URIs, bookmarks, docs in the wild) keeps working untouched.
const redditAuthHandler = (_req: Request, res: Response): void => {
  if (!REDDIT_CLIENT_ID) {
    res.status(503).send("REDDIT_CLIENT_ID not configured");
    return;
  }
  // #297: generate a random per-flow state and persist it; validated on the
  // callback to defeat OAuth CSRF / code-fixation (was hardcoded "augmentagent").
  const state = newRedditState();
  execFile(
    cliPath(),
    [
      "reddit",
      "auth-url",
      "--client-id",
      REDDIT_CLIENT_ID,
      "--redirect-uri",
      REDDIT_REDIRECT,
      "--state",
      state,
    ],
    (err, stdout) => {
      if (err) {
        res.status(500).send(`auth-url failed: ${err.message}`);
        return;
      }
      res.redirect(stdout.trim());
    }
  );
};

const redditCallbackHandler = (req: Request, res: Response): void => {
  const code = String(req.query.code || "");
  if (!code) {
    res.status(400).send("missing ?code");
    return;
  }
  // #297: validate the per-flow state before exchanging the code.
  const state = String(req.query.state || "");
  if (!consumeRedditState(state)) {
    res.status(403).send("invalid or missing OAuth state");
    return;
  }
  execFile(
    cliPath(),
    [
      "reddit",
      "exchange",
      "--client-id",
      REDDIT_CLIENT_ID,
      "--code",
      code,
      "--redirect-uri",
      REDDIT_REDIRECT,
    ],
    (err, stdout) => {
      if (err) {
        res.status(500).send(`token exchange failed: ${err.message}`);
        return;
      }
      res.send(
        "Reddit connected. The daemon will start polling your inbox on next restart. " +
          stdout.trim()
      );
    }
  );
};

reddit.get("/api/reddit/auth", redditAuthHandler);
reddit.get("/api/reddit/callback", redditCallbackHandler);

// Canonical `/oauth/<slug>/start` + `/oauth/<slug>/callback` aliases (issue
// #34). Same handlers, same redirect-URI env (`REDDIT_REDIRECT` still defaults
// to the legacy `/api/reddit/callback` path, so a Reddit app registered
// against the old URI keeps round-tripping correctly through either entry).
reddit.get("/oauth/reddit/start", redditAuthHandler);
reddit.get("/oauth/reddit/callback", redditCallbackHandler);

apiV1Router.use(reddit);


export default apiV1Router;
