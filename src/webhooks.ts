// #49 — dev-tool notification webhooks: Linear / Notion / Calendly.
//
// Each endpoint verifies the provider's signature with HMAC-SHA256 (or a
// static verification token, for Notion workspaces configured that way) over
// the RAW request body, then enqueues the event for the Rust daemon to pick
// up via the shared work pipeline. The HMAC computation here mirrors the Rust
// `augmentagent-channel-linear::hmac_sha256_hex` exactly so the polling
// fallback and the webhook converge on identical WorkItems.
//
// Raw-body capture: signature verification needs the bytes as-received, so
// these routes use express.raw() — they must be mounted BEFORE any global
// express.json() for these paths (handled by mounting this router early and
// scoping raw() per-route).
//
// Enqueue strategy: events are appended as rows the daemon already knows how
// to drain (the `emails`/work table the channels write). To stay
// dependency-light and avoid duplicating the Rust schema in TS, we persist a
// compact JSON line to a spool file the daemon tails; if the daemon-side
// consumer isn't wired yet the webhook still 200s (provider retries are
// avoided) and the polling fallback covers the gap.

import { Router } from "express";
import express from "express";
import crypto from "crypto";
import fs from "fs";
import path from "path";
import os from "os";
import { getConfig, insertSocialApiWebhookEvent } from "./db";

// Per-user state dir (0700), NOT world-writable /tmp. Prefer the runtime dir
// when set (matches the Rust crates' UDS/state convention), else fall back to
// ~/.local/state/augmentagent. Keeps the spool out of a predictable, shared,
// sticky-bit path an unprivileged local process could pre-plant or poison.
function stateDir(): string {
  const runtime = process.env.XDG_RUNTIME_DIR;
  if (runtime) return path.join(runtime, "augmentagent");
  return path.join(os.homedir(), ".local", "state", "augmentagent");
}

const SPOOL =
  process.env.AUGMENTAGENT_WEBHOOK_SPOOL ||
  path.join(stateDir(), "augmentagent-webhooks.jsonl");

// Append flags that REFUSE to follow a symlink at the target (O_NOFOLLOW →
// openSync throws ELOOP instead of redirecting our writes into an attacker
// file) and create the spool 0600 so only the owner can read/write it.
const SPOOL_APPEND_FLAGS =
  fs.constants.O_APPEND |
  fs.constants.O_CREAT |
  fs.constants.O_WRONLY |
  fs.constants.O_NOFOLLOW;

// Append `data` to `file` with the hardened flags above, creating the parent
// state dir 0700 first (recursive, ignore-if-exists). Throws on symlink/ELOOP
// so callers' existing try/catch logs the refusal and the route still 200s.
function appendHardened(file: string, data: string): void {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const fd = fs.openSync(file, SPOOL_APPEND_FLAGS, 0o600);
  try {
    fs.writeSync(fd, data);
  } finally {
    fs.closeSync(fd);
  }
}

function hmacHex(secret: string, body: Buffer): string {
  return crypto.createHmac("sha256", secret).update(body).digest("hex");
}

function timingSafeEqualStr(a: string, b: string): boolean {
  const ab = Buffer.from(a);
  const bb = Buffer.from(b);
  // Hash both sides to a fixed width first so the length check itself does
  // not leak the secret length via early return / timing.
  const ah = crypto.createHash("sha256").update(ab).digest();
  const bh = crypto.createHash("sha256").update(bb).digest();
  const eq = crypto.timingSafeEqual(ah, bh);
  // Still require true equality (sha256 collision resistance makes the hash
  // compare sufficient, but keep the direct check as defense in depth).
  return eq && ab.length === bb.length && crypto.timingSafeEqual(ab, bb);
}

function spool(platform: string, payload: unknown): void {
  try {
    appendHardened(
      SPOOL,
      JSON.stringify({ platform, at: Date.now(), payload }) + "\n"
    );
  } catch (e) {
    console.warn(`[webhooks] spool write failed: ${(e as Error).message}`);
  }
}

// ---------------------------------------------------------------------------
// Deft (Deftform) webhook receiver — #116.
//
// Unlike Linear/Notion/Calendly, Deftform's outbound webhook is **unsigned**
// (no HMAC/signature is documented — see `docs/deft-protocol.md` §4). The
// only authentication available is therefore a shared secret carried in the
// URL path: `/webhooks/deft/:secret`, constant-time-compared against
// `AUGMENTAGENT_DEFT_WEBHOOK_SECRET`. The whole path is inert unless both
// `AUGMENTAGENT_DEFT_ENABLED` is truthy AND the secret is set, mirroring the
// Rust crate's `deft_enabled()` arming gate (least-privilege for a C&C
// surface, not a ban-risk gate — the Deftform API is sanctioned).
//
// On a valid submission we normalize each `data[]` element to the same
// WorkItem/command shape the Rust deft poll path produces
// (`platform="deft"`, `kind="dm"`, `external_id="deft:<uuid>"`), dedup by
// submission `uuid` against a persisted seen-set so a webhook+poll (or a
// Deftform retry) double-delivery collapses to one spooled action, and hand
// off via the same spool the daemon already tails. `Store::is_message_processed`
// (keyed on the identical `deft:<uuid>`) is the authoritative downstream
// backstop; this receiver-level dedup just avoids redundant spool churn.

const DEFT_SEEN =
  process.env.AUGMENTAGENT_DEFT_SEEN ||
  path.join(stateDir(), "augmentagent-deft-seen.jsonl");

// Bound the in-memory mirror so a long-lived process can't grow unboundedly;
// the persisted file + the daemon's is_message_processed remain authoritative
// across the (rare) eviction window.
const DEFT_SEEN_MAX = 5000;
const deftSeenMem = new Set<string>();
let deftSeenLoaded = false;

function loadDeftSeen(): void {
  if (deftSeenLoaded) return;
  deftSeenLoaded = true;
  try {
    const raw = fs.readFileSync(DEFT_SEEN, "utf8");
    for (const line of raw.split("\n")) {
      const id = line.trim();
      if (id) deftSeenMem.add(id);
    }
  } catch {
    // No seen file yet — first run.
  }
}

// Returns true if this dedup id was already seen (and records it if not).
function deftMarkSeen(dedupId: string): boolean {
  loadDeftSeen();
  if (deftSeenMem.has(dedupId)) return true;
  deftSeenMem.add(dedupId);
  if (deftSeenMem.size > DEFT_SEEN_MAX) {
    // Evict oldest insertion (Set preserves insertion order).
    const oldest = deftSeenMem.values().next().value;
    if (oldest !== undefined) deftSeenMem.delete(oldest);
  }
  try {
    appendHardened(DEFT_SEEN, dedupId + "\n");
  } catch (e) {
    console.warn(`[webhooks] deft seen write failed: ${(e as Error).message}`);
  }
  return false;
}

// Mirror of the Rust `DeftSubmission` shape (defensive — docs don't pin the
// envelope, see `docs/deft-protocol.md` §4). One Deftform webhook body may
// carry a single submission object or a `data`/`submissions` array of them.
interface DeftField {
  label?: string;
  response?: string;
  uuid?: string;
  custom_key?: string;
}
interface DeftSubmission {
  submission_id?: string;
  id?: string;
  uuid?: string;
  formId?: string;
  form_id?: string;
  created_at?: string;
  submitted_at?: string;
  data?: DeftField[];
}

// Pull submissions out of whatever frame Deftform sends. A bare submission
// has a `data[]` of fields; a wrapper has a `data`/`submissions` array of
// submissions. Disambiguate by whether the array elements look like fields
// (have `label`/`response`) vs submissions (have `uuid`/`data`).
function extractDeftSubmissions(body: unknown): DeftSubmission[] {
  if (!body || typeof body !== "object") return [];
  const obj = body as Record<string, unknown>;
  const arr =
    (Array.isArray(obj.submissions) && obj.submissions) ||
    (Array.isArray(obj.data) &&
      obj.data.some(
        (e) =>
          e &&
          typeof e === "object" &&
          ("uuid" in (e as object) || "data" in (e as object)) &&
          !("response" in (e as object))
      ) &&
      obj.data) ||
    null;
  if (arr) return (arr as DeftSubmission[]).filter((s) => s && typeof s === "object");
  // Otherwise treat the body itself as one submission.
  return [obj as DeftSubmission];
}

function deftDedupId(sub: DeftSubmission, formId: string): string {
  const uuid = (sub.uuid || "").trim();
  if (uuid) return `deft:${uuid}`;
  const sid = (sub.submission_id || sub.id || "").trim();
  return `deft:${formId}:${sid}`;
}

const router = Router();

router.post(
  "/webhooks/deft/:secret",
  express.raw({ type: "*/*" }),
  (req, res) => {
    const secret = process.env.AUGMENTAGENT_DEFT_WEBHOOK_SECRET || "";
    const enabled = ["1", "true", "yes", "on"].includes(
      String(process.env.AUGMENTAGENT_DEFT_ENABLED || "")
        .trim()
        .toLowerCase()
    );
    // Inert unless armed AND a secret is configured. Same 404-ish 401 for
    // "not armed" and "bad secret" so an attacker can't probe the arming
    // state.
    const provided = String(req.params.secret || "");
    if (!enabled || !secret || !timingSafeEqualStr(provided, secret)) {
      res.status(401).json({ error: "bad secret" });
      return;
    }
    const raw: Buffer = req.body instanceof Buffer ? req.body : Buffer.from("");
    let parsed: unknown = {};
    try {
      parsed = JSON.parse(raw.toString("utf8"));
    } catch {
      res.status(400).json({ error: "bad json" });
      return;
    }
    const subs = extractDeftSubmissions(parsed);
    let accepted = 0;
    let duplicates = 0;
    for (const sub of subs) {
      const formId = (sub.formId || sub.form_id || "").trim();
      const dedupId = deftDedupId(sub, formId);
      if (deftMarkSeen(dedupId)) {
        duplicates++;
        continue;
      }
      // Stamp the form id from nothing-but-the-body for now; the Rust
      // handler re-derives the command from the spooled submission and
      // applies the same `into_email` normalization the poll path uses, so
      // the receiver only needs to forward the raw submission verbatim.
      spool("deft", sub);
      accepted++;
    }
    // Reply fast + 202 even on all-duplicate so Deftform (whose retry policy
    // is undocumented) does not hammer us; the body reports the split.
    res.status(202).json({ ok: true, accepted, duplicates });
  }
);

// Linear: `linear-signature` header = hex HMAC-SHA256 of the raw body, keyed
// by LINEAR_WEBHOOK_SECRET (falls back to LINEAR_API_KEY).
router.post(
  "/webhooks/linear",
  express.raw({ type: "*/*" }),
  (req, res) => {
    const secret =
      process.env.LINEAR_WEBHOOK_SECRET || process.env.LINEAR_API_KEY || "";
    const provided = String(req.header("linear-signature") || "");
    const raw: Buffer = req.body instanceof Buffer ? req.body : Buffer.from("");
    if (!secret || !timingSafeEqualStr(hmacHex(secret, raw), provided)) {
      res.status(401).json({ error: "bad signature" });
      return;
    }
    let parsed: unknown = {};
    try {
      parsed = JSON.parse(raw.toString("utf8"));
    } catch {}
    spool("linear", parsed);
    res.status(202).json({ ok: true });
  }
);

// Notion: either a static `notion-webhook-secret` header (token model) or an
// HMAC `x-notion-signature`. Accept either.
router.post(
  "/webhooks/notion",
  express.raw({ type: "*/*" }),
  (req, res) => {
    const secret = process.env.NOTION_WEBHOOK_SECRET || "";
    const raw: Buffer = req.body instanceof Buffer ? req.body : Buffer.from("");
    const tokenHeader = String(req.header("notion-webhook-secret") || "");
    const sigHeader = String(req.header("x-notion-signature") || "");
    const tokenOk =
      secret.length > 0 && timingSafeEqualStr(tokenHeader, secret);
    const hmacOk =
      secret.length > 0 &&
      sigHeader.length > 0 &&
      timingSafeEqualStr(hmacHex(secret, raw), sigHeader);
    if (!tokenOk && !hmacOk) {
      res.status(401).json({ error: "bad signature" });
      return;
    }
    let parsed: unknown = {};
    try {
      parsed = JSON.parse(raw.toString("utf8"));
    } catch {}
    spool("notion", parsed);
    res.status(202).json({ ok: true });
  }
);

// Calendly: `calendly-webhook-signature: t=<ts>,v1=<hmac>` over `{t}.{body}`.
router.post(
  "/webhooks/calendly",
  express.raw({ type: "*/*" }),
  (req, res) => {
    const secret = process.env.CALENDLY_WEBHOOK_SECRET || "";
    const raw: Buffer = req.body instanceof Buffer ? req.body : Buffer.from("");
    const header = String(req.header("calendly-webhook-signature") || "");
    const parts = Object.fromEntries(
      header.split(",").map((p) => {
        const i = p.indexOf("=");
        return [p.slice(0, i).trim(), p.slice(i + 1).trim()];
      })
    );
    const t = parts["t"];
    const v1 = parts["v1"];
    let ok = false;
    if (secret && t && v1) {
      const signed = Buffer.concat([Buffer.from(`${t}.`), raw]);
      ok = timingSafeEqualStr(hmacHex(secret, signed), v1);
    }
    if (!ok) {
      res.status(401).json({ error: "bad signature" });
      return;
    }
    let parsed: unknown = {};
    try {
      parsed = JSON.parse(raw.toString("utf8"));
    } catch {}
    spool("calendly", parsed);
    res.status(202).json({ ok: true });
  }
);

// ---------------------------------------------------------------------------
// SocialAPI.ai inbound webhook receiver — #249 (near-real-time inbox).
//
// SocialAPI.ai can PUSH new comment/DM events to us so engagement doesn't have
// to wait for the next poll tick. This receiver:
//   1. verifies an HMAC-SHA256 signature over the RAW body, keyed by the secret
//      from config (`socialapi_webhook_secret`) or env
//      `SOCIALAPI_WEBHOOK_SECRET`; rejects 401 on mismatch and FAILS CLOSED
//      (401) when no secret is configured — an unauthenticated push surface is
//      never opened by accident,
//   2. parses each `comment` / `dm` event out of the body, and
//   3. persists it idempotently into `socialapi_webhook_events` (de-duped by
//      provider event id) for the Rust daemon to DRAIN as a fast-path.
//
// It does NOT itself triage/draft — it only durably hands the event off. The
// Rust side reuses socialapi_seen_{dms,comments} so a webhook-delivered item
// and a later poll of the same item collapse to a single draft. The signature
// header is `x-socialapi-signature` (hex HMAC) — fall back to `sha256=<hex>`
// GitHub-style framing if present.
//
// Event shapes accepted (a single event object, or `{events:[...]}` /
// `{data:[...]}`, or a bare top-level array). Each event is normalized so the
// stored payload_json is exactly what the Rust drain needs to rebuild a
// WorkItem payload without another API call:
//   DM:      { type:"dm",      id, conversation_id, account_id, with, author,
//              text, created_at }
//   Comment: { type:"comment", id, post_id, author, text, created_at }

interface NormalizedWebhookEvent {
  kind: "dm" | "comment";
  id: string;
  payload: Record<string, unknown>;
  accountId: string | null;
}

function socialapiWebhookSecret(): string {
  return getConfig("socialapi_webhook_secret") || process.env.SOCIALAPI_WEBHOOK_SECRET || "";
}

// Pull the signature out of either `x-socialapi-signature` (bare hex) or a
// GitHub-style `sha256=<hex>` value in the same header.
function extractSig(raw: string): string {
  const v = raw.trim();
  const eq = v.indexOf("=");
  if (eq > 0 && v.slice(0, eq).toLowerCase() === "sha256") return v.slice(eq + 1).trim();
  return v;
}

function eventType(ev: Record<string, unknown>): "dm" | "comment" | null {
  const t = String(ev.type || ev.kind || ev.event || "").toLowerCase();
  if (t === "dm" || t === "direct_message" || t === "message") return "dm";
  if (t === "comment" || t === "own_post_comment") return "comment";
  // Infer from shape when the discriminator is absent.
  if ("conversation_id" in ev || "message_id" in ev) return "dm";
  if ("post_id" in ev || "comment_id" in ev) return "comment";
  return null;
}

function str(ev: Record<string, unknown>, ...keys: string[]): string {
  for (const k of keys) {
    const v = ev[k];
    if (typeof v === "string" && v.length > 0) return v;
    if (typeof v === "number") return String(v);
  }
  return "";
}

// True when the push explicitly says this message is OURS (outbound). Covers
// the shapes providers commonly use; absent/unrecognized means "not stated",
// which is NOT the same as "inbound" — the Rust drain treats an unstated
// direction with no registered handles as unattributable and defers to the
// poll path rather than guessing (#526).
function isOutbound(ev: Record<string, unknown>): boolean {
  for (const k of ["is_outbound", "outbound", "from_me", "is_from_me", "outgoing"]) {
    const v = ev[k];
    if (typeof v === "boolean") return v;
  }
  const dir = String(ev.direction ?? ev.dir ?? "").toLowerCase();
  return dir === "outbound" || dir === "out" || dir === "sent";
}

// Normalize one raw event into the canonical shape the Rust drain consumes.
// Returns null if it can't be classified or lacks a usable id (so the receiver
// skips it rather than persisting an unprocessable row).
function normalizeSocialApiEvent(ev: Record<string, unknown>): NormalizedWebhookEvent | null {
  const kind = eventType(ev);
  if (!kind) return null;
  if (kind === "dm") {
    const messageId = str(ev, "message_id", "id");
    const conversationId = str(ev, "conversation_id", "thread_id");
    if (!messageId || !conversationId) return null;
    const accountId = str(ev, "account_id") || null;
    return {
      kind,
      id: `socialapi:dm:${conversationId}:${messageId}`,
      accountId,
      payload: {
        type: "dm",
        id: messageId,
        conversation_id: conversationId,
        account_id: accountId || "",
        // #526: `with` is the counterparty, and it is DISPLAY DATA ONLY —
        // direction is decided in Rust against the registered account handles
        // and the `outbound` flag below, never against this field. No alias
        // fallback: `author` would be right only for inbound, and
        // `recipient`/`to` is the destination, which on an inbound DM is US.
        // Leave it empty when the push doesn't say and let the Rust side
        // pick a display fallback it can reason about.
        with: str(ev, "with"),
        author: str(ev, "author", "from"),
        // Explicit direction when the provider states it. This is the
        // authoritative ownership signal; the handle comparison is the
        // fallback for pushes that don't carry it.
        outbound: isOutbound(ev),
        // Which social network this actually came from. One SocialAPI.ai key
        // fronts several connected accounts, so without this the approval
        // card can only say "DM from jane" with no way to tell which inbox.
        sub_platform: str(ev, "platform", "network", "provider"),
        text: str(ev, "text", "body", "message"),
        created_at: str(ev, "created_at", "timestamp", "ts"),
      },
    };
  }
  // comment
  const commentId = str(ev, "comment_id", "id");
  const postId = str(ev, "post_id");
  if (!commentId || !postId) return null;
  return {
    kind,
    id: `socialapi:comment:${postId}:${commentId}`,
    accountId: str(ev, "account_id") || null,
    payload: {
      type: "comment",
      id: commentId,
      post_id: postId,
      // Which network the comment landed on — see the DM branch above.
      sub_platform: str(ev, "platform", "network", "provider"),
      author: str(ev, "author", "from"),
      text: str(ev, "text", "body", "message"),
      created_at: str(ev, "created_at", "timestamp", "ts"),
    },
  };
}

function extractSocialApiEvents(body: unknown): Record<string, unknown>[] {
  if (Array.isArray(body)) return body.filter((e) => e && typeof e === "object") as Record<string, unknown>[];
  if (!body || typeof body !== "object") return [];
  const obj = body as Record<string, unknown>;
  const arr =
    (Array.isArray(obj.events) && obj.events) ||
    (Array.isArray(obj.data) && obj.data) ||
    null;
  if (arr) return (arr as unknown[]).filter((e) => e && typeof e === "object") as Record<string, unknown>[];
  return [obj];
}

router.post("/webhooks/socialapi", express.raw({ type: "*/*" }), (req, res) => {
  const secret = socialapiWebhookSecret();
  const raw: Buffer = req.body instanceof Buffer ? req.body : Buffer.from("");
  const provided = extractSig(String(req.header("x-socialapi-signature") || ""));
  // Fail CLOSED: no secret configured → never accept an unauthenticated push.
  if (!secret || !provided || !timingSafeEqualStr(hmacHex(secret, raw), provided)) {
    res.status(401).json({ error: "bad signature" });
    return;
  }
  let parsed: unknown = {};
  try {
    parsed = JSON.parse(raw.toString("utf8"));
  } catch {
    res.status(400).json({ error: "bad json" });
    return;
  }
  let accepted = 0;
  let duplicates = 0;
  let skipped = 0;
  for (const ev of extractSocialApiEvents(parsed)) {
    const norm = normalizeSocialApiEvent(ev);
    if (!norm) {
      skipped++;
      continue;
    }
    try {
      const isNew = insertSocialApiWebhookEvent(
        norm.id,
        norm.kind,
        norm.accountId,
        JSON.stringify(norm.payload)
      );
      if (isNew) accepted++;
      else duplicates++;
    } catch (e) {
      console.warn(`[webhooks] socialapi persist failed: ${(e as Error).message}`);
      skipped++;
    }
  }
  // 202 even on all-duplicate so the provider doesn't retry-storm us; the body
  // reports the split. The Rust daemon drains accepted rows on its next tick.
  res.status(202).json({ ok: true, accepted, duplicates, skipped });
});

export default router;
export { hmacHex, SPOOL, DEFT_SEEN, normalizeSocialApiEvent, extractSocialApiEvents };
