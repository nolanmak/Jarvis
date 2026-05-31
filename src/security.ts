// src/security.ts — dashboard hardening for issue #297.
//
// The dashboard approves + sends mail on the operator's behalf. It previously
// bound 0.0.0.0 with no auth, no CSRF/Origin checks, and no CSP, so any local
// process — or any web page the operator visited (DNS-rebinding / CSRF) — could
// drive approve-send, rewrite recipients, or write provider credentials.
//
// This module centralizes the defenses so both entrypoints (src/index.ts and
// src/dashboard-server.ts) and both routers (UI + /api/v1) share one
// implementation:
//
//   - getBindHost():        bind 127.0.0.1 by default, AUGMENTAGENT_BIND_HOST knob.
//   - getDashboardPort():   single source for the port.
//   - resolveApiKey():      fail-closed local mode — load or generate+persist a
//                           key on first run and log it once.
//   - requireAuth:          Bearer header OR signed session cookie (browser).
//   - loginHandler:         exchanges a valid key for a session cookie.
//   - hostOriginGuard:      Host allow-list (anti DNS-rebinding) + Origin/Referer
//                           allow-list on state-changing methods (anti CSRF).
//   - contentSecurityPolicy: strict CSP header.

import { createHmac, randomBytes, timingSafeEqual } from "crypto";
import type { Request, Response, NextFunction } from "express";
import { getConfig, setConfig } from "./db";

export const MODE = (process.env.MODE || "local").toLowerCase();

const DEFAULT_HOST = "127.0.0.1";
const SESSION_COOKIE = "augmentagent_session";
const SESSION_TTL_MS = 12 * 60 * 60 * 1000; // 12h
const CONFIG_API_KEY = "dashboard_api_key";
const CONFIG_SESSION_SECRET = "dashboard_session_secret";

export function getDashboardPort(): number {
  return parseInt(process.env.DASHBOARD_PORT || "3000", 10);
}

/** Bind 127.0.0.1 by default; overridable for split-mode behind a reverse
 *  proxy/TLS. Never silently binds 0.0.0.0 just because PORT was set. */
export function getBindHost(): string {
  return process.env.AUGMENTAGENT_BIND_HOST || DEFAULT_HOST;
}

// --- API key: fail-closed local mode -------------------------------------
//
// Resolution order:
//   1. AUGMENTAGENT_API_KEY env (operator-provided, also used in split mode).
//   2. Persisted key in the DB config table (generated on a prior run).
//   3. Generate a fresh key, persist it, and log it once.
//
// The result is that auth is ALWAYS on. There is no no-op path.
let cachedApiKey: string | null = null;

export function resolveApiKey(): string {
  if (cachedApiKey) return cachedApiKey;

  const envKey = (process.env.AUGMENTAGENT_API_KEY || "").trim();
  if (envKey) {
    cachedApiKey = envKey;
    return cachedApiKey;
  }

  const persisted = (getConfig(CONFIG_API_KEY) || "").trim();
  if (persisted) {
    cachedApiKey = persisted;
    return cachedApiKey;
  }

  const generated = randomBytes(32).toString("hex");
  setConfig(CONFIG_API_KEY, generated);
  cachedApiKey = generated;
  // Log ONCE so the operator can copy it. This is the only place the key is
  // printed; subsequent runs read it back from the DB silently.
  console.log(
    "\n========================================================================\n" +
      "[security] No AUGMENTAGENT_API_KEY set — generated and persisted one.\n" +
      `[security] Dashboard API key: ${generated}\n` +
      "[security] Use it as `Authorization: Bearer <key>` or log in at /login.\n" +
      "[security] Set AUGMENTAGENT_API_KEY to override.\n" +
      "========================================================================\n"
  );
  return cachedApiKey;
}

function sessionSecret(): string {
  const existing = (getConfig(CONFIG_SESSION_SECRET) || "").trim();
  if (existing) return existing;
  const secret = randomBytes(32).toString("hex");
  setConfig(CONFIG_SESSION_SECRET, secret);
  return secret;
}

function constantTimeEqual(a: string, b: string): boolean {
  const ab = Buffer.from(a);
  const bb = Buffer.from(b);
  if (ab.length !== bb.length) return false;
  return timingSafeEqual(ab, bb);
}

// --- Signed session cookie (browser auth) --------------------------------
//
// A browser navigating to the UI can't send an Authorization header, so the
// operator logs in once (POST /login with the key) and receives an HMAC-signed,
// HttpOnly, SameSite=Strict cookie scoped to localhost. Format: <expiry>.<sig>.
function signSession(expiry: number): string {
  const sig = createHmac("sha256", sessionSecret())
    .update(String(expiry))
    .digest("hex");
  return `${expiry}.${sig}`;
}

function verifySession(token: string): boolean {
  const dot = token.indexOf(".");
  if (dot <= 0) return false;
  const expiryStr = token.slice(0, dot);
  const sig = token.slice(dot + 1);
  const expiry = parseInt(expiryStr, 10);
  if (!Number.isFinite(expiry) || expiry < Date.now()) return false;
  const expected = createHmac("sha256", sessionSecret())
    .update(expiryStr)
    .digest("hex");
  return constantTimeEqual(sig, expected);
}

function parseCookies(header: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (!header) return out;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    const k = part.slice(0, eq).trim();
    const v = part.slice(eq + 1).trim();
    if (k) out[k] = decodeURIComponent(v);
  }
  return out;
}

function bearerToken(req: Request): string | null {
  const h = req.header("authorization") || "";
  if (h.toLowerCase().startsWith("bearer ")) return h.slice(7).trim();
  // Back-compat: the machine API + CLI historically used x-api-key.
  const x = req.header("x-api-key");
  return x ? x.trim() : null;
}

/** Auth middleware applied to BOTH routers. Accepts a Bearer/x-api-key header
 *  (machine clients) OR a valid signed session cookie (browser). Fail-closed:
 *  there is always a key to match against. */
export function requireAuth(req: Request, res: Response, next: NextFunction): void {
  const key = resolveApiKey();

  const token = bearerToken(req);
  if (token && constantTimeEqual(token, key)) {
    next();
    return;
  }

  const cookies = parseCookies(req.header("cookie"));
  const session = cookies[SESSION_COOKIE];
  if (session && verifySession(session)) {
    next();
    return;
  }

  // Browsers navigating the UI get a login redirect; API clients get JSON 401.
  const wantsHtml = (req.header("accept") || "").includes("text/html");
  if (wantsHtml && req.method === "GET") {
    res.redirect("/login");
    return;
  }
  res.status(401).json({ error: "authentication required" });
}

/** GET /login — minimal form. POST /login — exchange key for a session cookie. */
export function loginPageHandler(_req: Request, res: Response): void {
  res
    .status(200)
    .type("html")
    .send(
      "<!doctype html><meta charset=utf-8>" +
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'self'\">" +
        "<title>AugmentAgent login</title>" +
        "<form method=post action=/login>" +
        "<p>Dashboard API key:</p>" +
        "<input name=key type=password autofocus style='width:24rem'>" +
        "<button type=submit>Log in</button>" +
        "</form>"
    );
}

export function loginSubmitHandler(req: Request, res: Response): void {
  const provided = String((req.body && req.body.key) || "").trim();
  const key = resolveApiKey();
  if (!provided || !constantTimeEqual(provided, key)) {
    res.status(401).type("html").send("Invalid key. <a href=/login>Try again</a>.");
    return;
  }
  const expiry = Date.now() + SESSION_TTL_MS;
  const cookie = signSession(expiry);
  const secure = (process.env.AUGMENTAGENT_COOKIE_SECURE || "").toLowerCase() === "true";
  res.setHeader(
    "Set-Cookie",
    `${SESSION_COOKIE}=${encodeURIComponent(cookie)}; HttpOnly; SameSite=Strict; Path=/; Max-Age=${Math.floor(
      SESSION_TTL_MS / 1000
    )}${secure ? "; Secure" : ""}`
  );
  res.redirect("/");
}

// --- Host + Origin guard (anti DNS-rebinding / anti CSRF) -----------------
//
// allow-list of host:port values that may reach the dashboard. Defaults cover
// the loopback names a same-origin browser uses; AUGMENTAGENT_ALLOWED_HOSTS adds
// the public hostname for split-mode reverse-proxy deployments.
function allowedHosts(): Set<string> {
  const port = getDashboardPort();
  const base = [
    `localhost:${port}`,
    `127.0.0.1:${port}`,
    `[::1]:${port}`,
    "localhost",
    "127.0.0.1",
  ];
  const extra = (process.env.AUGMENTAGENT_ALLOWED_HOSTS || "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return new Set([...base, ...extra].map((h) => h.toLowerCase()));
}

function hostOf(value: string): string {
  try {
    return new URL(value).host.toLowerCase();
  } catch {
    return "";
  }
}

/** Validates the Host header against the allow-list on every request (stops
 *  DNS-rebinding, where an attacker-controlled name resolves to 127.0.0.1), and
 *  the Origin/Referer on state-changing methods (stops cross-origin CSRF POSTs
 *  from any page the operator visits). */
export function hostOriginGuard(req: Request, res: Response, next: NextFunction): void {
  const allow = allowedHosts();

  const host = (req.headers.host || "").toLowerCase();
  if (!host || !allow.has(host)) {
    res.status(403).json({ error: "host not allowed" });
    return;
  }

  const stateChanging =
    req.method === "POST" || req.method === "PUT" || req.method === "DELETE" || req.method === "PATCH";
  if (stateChanging) {
    const origin = req.header("origin");
    const referer = req.header("referer");
    if (origin) {
      if (!allow.has(hostOf(origin))) {
        res.status(403).json({ error: "cross-origin request rejected" });
        return;
      }
    } else if (referer) {
      if (!allow.has(hostOf(referer))) {
        res.status(403).json({ error: "cross-origin request rejected" });
        return;
      }
    }
    // No Origin and no Referer: allowed only for same-origin non-browser
    // clients that already passed Bearer auth (curl/CLI). Browsers always send
    // at least one on cross-origin requests, so this does not weaken the CSRF
    // guard for the form-post / fetch vectors the issue describes.
  }
  next();
}

// --- Content-Security-Policy ---------------------------------------------
//
// Strict policy. The EJS views load htmx from unpkg, so that origin is allowed
// explicitly under script-src (per #297: "allow the unpkg origin explicitly in
// CSP" when self-hosting isn't trivial). 'unsafe-inline' is permitted for style
// only because the views use inline style attributes; scripts are otherwise
// locked to 'self' + unpkg. To self-host later, drop unpkg here and vendor the
// htmx bundle into public/.
export function contentSecurityPolicy(_req: Request, res: Response, next: NextFunction): void {
  res.setHeader(
    "Content-Security-Policy",
    [
      "default-src 'self'",
      "script-src 'self' https://unpkg.com",
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data:",
      "connect-src 'self'",
      "font-src 'self' data:",
      "frame-ancestors 'none'",
      "base-uri 'self'",
      "form-action 'self'",
      "object-src 'none'",
    ].join("; ")
  );
  res.setHeader("X-Content-Type-Options", "nosniff");
  res.setHeader("X-Frame-Options", "DENY");
  res.setHeader("Referrer-Policy", "same-origin");
  next();
}

// --- Reddit OAuth state (anti OAuth-CSRF / code-fixation) -----------------
//
// Per-flow random state, persisted (single-operator, single in-flight flow is
// the realistic case) and validated on callback.
const CONFIG_REDDIT_STATE = "reddit_oauth_state";

export function newRedditState(): string {
  const state = randomBytes(24).toString("hex");
  setConfig(CONFIG_REDDIT_STATE, state);
  return state;
}

export function consumeRedditState(provided: string): boolean {
  const stored = (getConfig(CONFIG_REDDIT_STATE) || "").trim();
  if (!stored || !provided) return false;
  const ok = constantTimeEqual(provided, stored);
  // One-time use: clear regardless so a captured state can't be replayed.
  setConfig(CONFIG_REDDIT_STATE, "");
  return ok;
}
