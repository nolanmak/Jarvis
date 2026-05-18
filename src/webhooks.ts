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

const SPOOL =
  process.env.AUGMENTAGENT_WEBHOOK_SPOOL ||
  path.join(os.tmpdir(), "augmentagent-webhooks.jsonl");

function hmacHex(secret: string, body: Buffer): string {
  return crypto.createHmac("sha256", secret).update(body).digest("hex");
}

function timingSafeEqualStr(a: string, b: string): boolean {
  const ab = Buffer.from(a);
  const bb = Buffer.from(b);
  if (ab.length !== bb.length) return false;
  return crypto.timingSafeEqual(ab, bb);
}

function spool(platform: string, payload: unknown): void {
  try {
    fs.appendFileSync(
      SPOOL,
      JSON.stringify({ platform, at: Date.now(), payload }) + "\n"
    );
  } catch (e) {
    console.warn(`[webhooks] spool write failed: ${(e as Error).message}`);
  }
}

const router = Router();

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

export default router;
export { hmacHex, SPOOL };
