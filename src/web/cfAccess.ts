// src/web/cfAccess.ts — verify the edge's Access JWT inside the app (spec D6).
//
// The edge decides who reaches the tunnel. This module makes the same decision
// for anything that reaches the dashboard some other way. Node's built-in
// `crypto` and `fetch` only:
//
//   - verifyAccessJwt(): pure check of one token against a set of keys.
//   - JwksCache:         keys from https://<team-domain>/cdn-cgi/access/certs,
//                        held for 1 h, refetched on an unknown kid at most once
//                        per 60 s. Throws JwksUnavailableError when the fetch
//                        fails and no cached key matches (caller fails closed).
//   - accessConfigFromEnv(): all-or-nothing AUGMENTAGENT_CF_ACCESS_* config.

import { createPublicKey, verify, type KeyObject } from "crypto";

export type AccessKeys = Record<string, KeyObject>;

export interface AccessConfig {
  aud: string;
  teamDomain: string;
  allowEmails: string[];
}

export interface VerifyOptions extends AccessConfig {
  /** Current time in ms. */
  now: number;
  keys: AccessKeys;
}

export type VerifyResult = { ok: true; email: string } | { ok: false; reason: string };

const SKEW_S = 60;
const KEY_TTL_MS = 60 * 60 * 1000;
const REFETCH_MIN_MS = 60 * 1000;
const FETCH_TIMEOUT_MS = 5000;

function decodePart(part: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(Buffer.from(part, "base64url").toString("utf8"));
    return v && typeof v === "object" && !Array.isArray(v) ? v : null;
  } catch {
    return null;
  }
}

/** The `kid` from a token's header, or null when the token is malformed. */
export function jwtKid(token: string): string | null {
  const header = decodePart(token.split(".")[0] || "");
  return header && typeof header.kid === "string" ? header.kid : null;
}

export function verifyAccessJwt(token: string, opts: VerifyOptions): VerifyResult {
  const parts = (token || "").split(".");
  if (parts.length !== 3) return { ok: false, reason: "malformed" };
  const header = decodePart(parts[0]);
  const claims = decodePart(parts[1]);
  if (!header || !claims) return { ok: false, reason: "malformed" };
  if (header.alg !== "RS256") return { ok: false, reason: "alg" };
  const key = typeof header.kid === "string" ? opts.keys[header.kid] : undefined;
  if (!key) return { ok: false, reason: "unknown_kid" };

  let valid = false;
  try {
    valid = verify("RSA-SHA256", Buffer.from(`${parts[0]}.${parts[1]}`), key, Buffer.from(parts[2], "base64url"));
  } catch {
    valid = false;
  }
  if (!valid) return { ok: false, reason: "signature" };

  const aud = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  if (!aud.includes(opts.aud)) return { ok: false, reason: "aud" };
  if (claims.iss !== `https://${opts.teamDomain}`) return { ok: false, reason: "iss" };

  const nowS = opts.now / 1000;
  if (typeof claims.exp !== "number" || nowS > claims.exp + SKEW_S) return { ok: false, reason: "expired" };
  if (claims.nbf !== undefined && (typeof claims.nbf !== "number" || nowS + SKEW_S < claims.nbf)) {
    return { ok: false, reason: "not_yet_valid" };
  }

  const email = typeof claims.email === "string" ? claims.email.toLowerCase() : "";
  if (!email || !opts.allowEmails.includes(email)) return { ok: false, reason: "email" };
  return { ok: true, email };
}

export class JwksUnavailableError extends Error {
  constructor(cause: string) {
    super(`Access keys unavailable: ${cause}`);
    this.name = "JwksUnavailableError";
  }
}

type FetchLike = (url: string, init?: { signal?: AbortSignal }) => Promise<{ ok: boolean; status: number; json(): Promise<unknown> }>;

export class JwksCache {
  private keys: AccessKeys = {};
  private fetchedAt = -Infinity;
  private lastAttempt = -Infinity;
  private lastError: string | null = null;
  private readonly url: string;
  private readonly fetchFn: FetchLike;
  private readonly now: () => number;

  constructor(opts: { teamDomain: string; fetch?: FetchLike; now?: () => number }) {
    this.url = `https://${opts.teamDomain}/cdn-cgi/access/certs`;
    // Resolve the global at call time so tests can stub it.
    this.fetchFn = opts.fetch || ((url, init) => globalThis.fetch(url, init));
    this.now = opts.now || Date.now;
  }

  /** Current keys, refreshed if `kid` is unknown or the set is stale. */
  async keysFor(kid: string): Promise<AccessKeys> {
    const t = this.now();
    if (t - this.fetchedAt >= KEY_TTL_MS) this.keys = {};
    if (!this.keys[kid] && t - this.lastAttempt >= REFETCH_MIN_MS) {
      this.lastAttempt = t;
      try {
        this.keys = await this.fetchKeys();
        this.fetchedAt = t;
        this.lastError = null;
      } catch (err) {
        this.lastError = err instanceof Error ? err.message : String(err);
      }
    }
    if (!this.keys[kid] && this.lastError !== null) throw new JwksUnavailableError(this.lastError);
    return this.keys;
  }

  private async fetchKeys(): Promise<AccessKeys> {
    const res = await this.fetchFn(this.url, { signal: AbortSignal.timeout(FETCH_TIMEOUT_MS) });
    if (!res.ok) throw new Error(`JWKS HTTP ${res.status}`);
    const body = (await res.json()) as { keys?: unknown };
    if (!Array.isArray(body?.keys)) throw new Error("JWKS has no keys");
    const out: AccessKeys = {};
    for (const jwk of body.keys as Array<Record<string, unknown>>) {
      if (!jwk || jwk.kty !== "RSA" || typeof jwk.kid !== "string") continue;
      try {
        out[jwk.kid] = createPublicKey({ key: jwk as any, format: "jwk" });
      } catch {
        // Skip a key Node can't parse; the rest of the set is still usable.
      }
    }
    return out;
  }
}

const ENV_AUD = "AUGMENTAGENT_CF_ACCESS_AUD";
const ENV_TEAM = "AUGMENTAGENT_CF_ACCESS_TEAM_DOMAIN";
const ENV_EMAILS = "AUGMENTAGENT_CF_ACCESS_EMAILS";

/** null when Access mode is off (none set). Throws when only some are set, or
 *  when a value is unusable, so the dashboard refuses to start. */
export function accessConfigFromEnv(env: NodeJS.ProcessEnv): AccessConfig | null {
  const aud = (env[ENV_AUD] || "").trim();
  const teamDomain = (env[ENV_TEAM] || "").trim().toLowerCase();
  const rawEmails = (env[ENV_EMAILS] || "").trim();
  const present = [aud, teamDomain, rawEmails].filter(Boolean).length;
  if (present === 0) return null;
  if (present < 3) {
    const missing = [
      [ENV_AUD, aud],
      [ENV_TEAM, teamDomain],
      [ENV_EMAILS, rawEmails],
    ]
      .filter(([, v]) => !v)
      .map(([k]) => k);
    throw new Error(`Access config is partial; set all of ${ENV_AUD}, ${ENV_TEAM}, ${ENV_EMAILS} or none. Missing: ${missing.join(", ")}`);
  }
  if (!/^[a-z0-9.-]+$/.test(teamDomain)) {
    throw new Error(`${ENV_TEAM} must be a bare host such as example.cloudflareaccess.com`);
  }
  const allowEmails = rawEmails
    .split(",")
    .map((e) => e.trim().toLowerCase())
    .filter(Boolean);
  if (allowEmails.length === 0) throw new Error(`${ENV_EMAILS} lists no email addresses`);
  return { aud, teamDomain, allowEmails };
}
