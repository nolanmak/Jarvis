#!/usr/bin/env node
// wix-events-sync.mjs — mirror a Meetup group's upcoming events onto a Wix
// Events calendar (#633).
//
// Sources:
//   - scripts/meetup-events.mjs (private GraphQL). When that fails with a
//     stale persisted-query hash — exit 2, which happens every time Meetup
//     ships a frontend build — it falls back to scripts/meetup-events-ssr.mjs,
//     which reads the same data out of the server-rendered page.
//   - scripts/luma-events.mjs (Luma's auth-free per-calendar ICS feed).
//
// Events cross-posted to both are deduped before anything is created.
//
// Sink: Wix Events v3 REST. CREATE ONLY. This job never updates and never
// deletes; a Meetup rename must not rewrite a hand-curated listing, and
// nothing here should ever be able to remove an event from a live site.
//
// Idempotent: it queries Wix first and creates only what is missing, so
// running it on a timer is safe.
//
// Usage:
//   node scripts/wix-events-sync.mjs                 plan only, no writes
//   node scripts/wix-events-sync.mjs --yes           create the missing events
//   node scripts/wix-events-sync.mjs --groups a,b    override the group list
//   node scripts/wix-events-sync.mjs --window-days 60
//
// Env (see .env.example):
//   AUGMENTAGENT_WIX_API_KEY           Wix account API key, Events read+manage
//   AUGMENTAGENT_WIX_SITE_ID           target Wix site
//   AUGMENTAGENT_WIX_MEETUP_GROUPS     comma-separated Meetup group slugs
//   AUGMENTAGENT_WIX_LUMA_CALENDARS    comma-separated Luma cal-… ids
//   AUGMENTAGENT_WIX_SYNC_DRY_RUN      default 1
//   AUGMENTAGENT_WIX_SYNC_MAX_PER_RUN  default 5
//   AUGMENTAGENT_WIX_SYNC_REQUIRE_APPROVAL  default 1 — a timer can only plan
//   AUGMENTAGENT_WIX_TIMEZONE          default America/New_York
//
// Exit codes: 0 ok · 1 error · 2 source unavailable (both fetchers failed)

import { execFile } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const run = promisify(execFile);
const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const EVENTS_ENDPOINT = "https://www.wixapis.com/events/v3/events";
const EVENTS_QUERY_ENDPOINT = `${EVENTS_ENDPOINT}/query`;
const DEFAULT_DURATION_MS = 2 * 60 * 60 * 1000;

function loadDotEnv() {
  try {
    const text = readFileSync(resolve(REPO_ROOT, ".env"), "utf8");
    for (const line of text.split("\n")) {
      const m = /^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$/.exec(line);
      if (m && !process.env[m[1]]) process.env[m[1]] = m[2].replace(/^["']|["']$/g, "");
    }
  } catch {
    /* env may come from systemd or the shell */
  }
}

const log = (msg) => console.log(msg);
const warn = (msg) => console.error(`\x1b[33m[wix-sync]\x1b[0m ${msg}`);
const die = (msg, code = 1) => {
  console.error(`\x1b[31m[wix-sync ERR]\x1b[0m ${msg}`);
  process.exit(code);
};

// ---------------------------------------------------------------- source

/**
 * Upcoming events for one group. Prefers the GraphQL client; falls back to the
 * SSR reader when the persisted-query hash has gone stale (exit 2).
 */
async function fetchGroup(urlname) {
  const attempt = async (script) => {
    const { stdout } = await run(
      process.execPath,
      [resolve(REPO_ROOT, "scripts", script), urlname, "--json"],
      { cwd: REPO_ROOT, maxBuffer: 32 * 1024 * 1024 },
    );
    return JSON.parse(stdout).events ?? [];
  };

  try {
    return { events: await attempt("meetup-events.mjs"), source: "graphql" };
  } catch (err) {
    if (err.code === 2) {
      warn(
        `${urlname}: persisted-query hash is stale — falling back to the SSR reader. ` +
          `Refresh the hash in scripts/meetup-events.mjs via /intercept.`,
      );
      return { events: await attempt("meetup-events-ssr.mjs"), source: "ssr" };
    }
    throw err;
  }
}

/** Upcoming events for one Luma calendar, via its public ICS feed. */
async function fetchLumaCalendar(calendarId) {
  const { stdout } = await run(
    process.execPath,
    [resolve(REPO_ROOT, "scripts", "luma-events.mjs"), calendarId, "--json"],
    { cwd: REPO_ROOT, maxBuffer: 32 * 1024 * 1024 },
  );
  return { events: JSON.parse(stdout).events ?? [], source: "luma" };
}

// ------------------------------------------------------------------ wix

function wixHeaders() {
  const apiKey = process.env.AUGMENTAGENT_WIX_API_KEY;
  const siteId = process.env.AUGMENTAGENT_WIX_SITE_ID;
  if (!apiKey || !siteId) {
    die(
      "missing AUGMENTAGENT_WIX_API_KEY / AUGMENTAGENT_WIX_SITE_ID.\n" +
        "  Generate a key at https://manage.wix.com/account/api-keys with Wix Events\n" +
        "  Manage Events + Read Events, then add both to .env. Nothing was sent to Wix.",
    );
  }
  return { Authorization: apiKey, "wix-site-id": siteId, "Content-Type": "application/json" };
}

async function wixPost(url, headers, body) {
  const res = await fetch(url, { method: "POST", headers, body: JSON.stringify(body) });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    if (res.status === 403) {
      throw new Error(
        `Wix returned 403 — check the key's Events permissions and that the site id matches.\n${detail}`,
      );
    }
    throw new Error(`Wix returned ${res.status}: ${detail.slice(0, 400)}`);
  }
  return res.json();
}

/**
 * Wix's filter parser rejects {field: {$gte, $lte}} on a DateTime field
 * ("Value 'Map($gte -> …, $lte -> …)' is invalid for field start"), so the
 * window has to be two $and clauses. paging.limit defaults to 0 and returns
 * nothing, so it is always set.
 */
function queryBody(from, to, limit = 100) {
  const field = "dateAndTimeSettings.startDate";
  return {
    query: {
      filter: { $and: [{ [field]: { $gte: from } }, { [field]: { $lte: to } }] },
      sort: [{ fieldName: field, order: "ASC" }],
      paging: { limit, offset: 0 },
    },
  };
}

function createBody(planned, timeZoneId) {
  const venue = planned.source.venue;
  return {
    event: {
      title: planned.title,
      location: planned.source.isOnline
        ? { name: "Online", type: "ONLINE" }
        : {
            name: venue?.name || "Blockspace",
            type: "VENUE",
            address: venue?.address ? { addressLine: venue.address } : undefined,
          },
      dateAndTimeSettings: {
        startDate: planned.startDate,
        endDate: planned.endDate,
        timeZoneId,
      },
      shortDescription: planned.shortDescription,
      registration: { initialType: "RSVP", rsvp: { responseType: "YES_ONLY" } },
    },
  };
}

// ------------------------------------------------------------- planning

/** Meetup's dateTime carries its own UTC offset, so no timezone guessing. */
function toPlanned(event) {
  const start = new Date(event.dateTime);
  const end = event.endTime ? new Date(event.endTime) : new Date(start.getTime() + DEFAULT_DURATION_MS);
  const blurb = (event.description || "").replace(/\s+/g, " ").trim().slice(0, 280);
  return {
    title: event.title,
    startDate: start.toISOString(),
    endDate: end.toISOString(),
    shortDescription: blurb ? `${blurb}${blurb.length === 280 ? "…" : ""}\n\n${event.url}` : event.url,
    label: `${start.toISOString().slice(0, 10)} ${event.title}`,
    source: event,
  };
}

/**
 * Titles are compared loosely across sources. Luma appends its calendar name
 * to every SUMMARY, and hosts rarely punctuate a cross-post identically, so an
 * exact match would create the same event twice on a live public calendar.
 */
export function normalizeTitle(title) {
  return (title ?? "")
    .toLowerCase()
    .replace(/[\u2010-\u2015]/g, "-")
    .replace(/[^a-z0-9]+/g, " ")
    .trim();
}

/**
 * Two listings are the same event when they start at the same minute and one
 * title contains the other. Containment rather than equality is deliberate:
 * it still collapses the cross-post if the calendar-name suffix survives the
 * source-side strip, and duplicating an event on a live public calendar is a
 * far worse failure than merging two that happen to start together.
 */
export function isSameListing(a, b) {
  if (Math.abs(Date.parse(a.startDate) - Date.parse(b.startDate)) >= 60_000) return false;
  const [x, y] = [normalizeTitle(a.title), normalizeTitle(b.title)];
  if (!x || !y) return false;
  return x === y || x.startsWith(y) || y.startsWith(x);
}

/** One event, two sources — keep the first and drop the rest. */
export function dedupeAcrossSources(planned) {
  const kept = [];
  for (const p of planned) {
    if (kept.some((k) => isSameListing(k, p))) continue;
    kept.push(p);
  }
  return kept;
}

/** Same title, start within a minute — tolerant of Wix echoing a different ISO form. */
function alreadyOnWix(planned, existing) {
  return existing.some((e) => {
    if (normalizeTitle(e.title) !== normalizeTitle(planned.title)) return false;
    const start = e.dateAndTimeSettings?.startDate;
    if (!start) return false;
    return Math.abs(Date.parse(start) - Date.parse(planned.startDate)) < 60_000;
  });
}

// ------------------------------------------------------------------ cli

function parseArgs(argv) {
  const flags = new Map();
  let yes = false;
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--yes" || a === "-y") yes = true;
    else if (a.startsWith("--")) flags.set(a.slice(2), argv[++i] ?? "");
  }
  return { yes, flags };
}

const envFlag = (name, fallback) => {
  const v = process.env[name];
  if (v === undefined || v === "") return fallback;
  return !/^(0|false|no)$/i.test(v);
};

async function main() {
  loadDotEnv();
  const { yes, flags } = parseArgs(process.argv.slice(2));

  const split = (v) => (v ?? "").split(",").map((x) => x.trim()).filter(Boolean);
  const groups = split(flags.get("groups") ?? process.env.AUGMENTAGENT_WIX_MEETUP_GROUPS);
  const calendars = split(flags.get("luma") ?? process.env.AUGMENTAGENT_WIX_LUMA_CALENDARS);
  if (groups.length === 0 && calendars.length === 0) {
    die("no sources configured — set AUGMENTAGENT_WIX_MEETUP_GROUPS and/or AUGMENTAGENT_WIX_LUMA_CALENDARS");
  }

  const windowDays = Number(flags.get("window-days") ?? 90);
  const maxPerRun = Number(flags.get("limit") ?? process.env.AUGMENTAGENT_WIX_SYNC_MAX_PER_RUN ?? 5);
  const timeZoneId = process.env.AUGMENTAGENT_WIX_TIMEZONE || "America/New_York";
  const dryRunDefault = envFlag("AUGMENTAGENT_WIX_SYNC_DRY_RUN", true);
  const requireApproval = envFlag("AUGMENTAGENT_WIX_SYNC_REQUIRE_APPROVAL", true);

  // Collect the source side first — a source failure must never look like
  // "nothing to do".
  const planned = [];
  let sourcesOk = 0;
  for (const urlname of groups) {
    try {
      const { events, source } = await fetchGroup(urlname);
      sourcesOk += 1;
      log(`${urlname}: ${events.length} upcoming event(s) via ${source}`);
      planned.push(...events.map(toPlanned));
    } catch (err) {
      warn(`${urlname}: source failed — ${err.message}`);
    }
  }
  for (const calendarId of calendars) {
    try {
      const { events } = await fetchLumaCalendar(calendarId);
      sourcesOk += 1;
      log(`${calendarId}: ${events.length} upcoming event(s) via luma`);
      planned.push(...events.map(toPlanned));
    } catch (err) {
      warn(`${calendarId}: Luma source failed — ${err.message}`);
    }
  }
  if (sourcesOk === 0) die("every source failed — not treating this as an empty calendar", 2);

  const now = new Date();
  const until = new Date(now.getTime() + windowDays * 86_400_000);
  const inWindow = dedupeAcrossSources(
    planned
      .filter((p) => Date.parse(p.startDate) >= now.getTime() && Date.parse(p.startDate) <= until.getTime())
      .sort((a, b) => Date.parse(a.startDate) - Date.parse(b.startDate)),
  );
  const crossPosted = planned.length - inWindow.length;

  const headers = wixHeaders();
  const existing =
    (await wixPost(EVENTS_QUERY_ENDPOINT, headers, queryBody(now.toISOString(), until.toISOString())))
      .events ?? [];

  const missingAll = inWindow.filter((p) => !alreadyOnWix(p, existing));
  const missing = missingAll.slice(0, maxPerRun);
  const heldBack = missingAll.length - missing.length;

  if (crossPosted > 0) log(`\n${crossPosted} cross-posted duplicate(s) collapsed.`);
  log(
    `\n${inWindow.length} upcoming in the next ${windowDays}d · ` +
      `${inWindow.length - missingAll.length} already on Wix · ${missingAll.length} missing\n`,
  );
  for (const p of inWindow.filter((x) => !missingAll.includes(x))) log(`  skip    ${p.label}`);
  for (const p of missing) log(`  create  ${p.label}`);
  if (heldBack > 0) log(`  (cap ${maxPerRun}: ${heldBack} more held back for the next run)`);

  if (missing.length === 0) return log("\nNothing to do.");

  if (!yes || (requireApproval && !yes)) {
    log(
      `\nPlan only — nothing was created.` +
        (requireApproval ? `\nAUGMENTAGENT_WIX_SYNC_REQUIRE_APPROVAL is on: a timer can only ever plan.` : "") +
        `\nRe-run with --yes to publish these to the live calendar.`,
    );
    return;
  }
  if (dryRunDefault && !yes) return log("\nDry run — nothing was created.");

  log("");
  for (const p of missing) {
    const res = await wixPost(EVENTS_ENDPOINT, headers, createBody(p, timeZoneId));
    log(`  created ${p.label}  (${res.event?.id ?? "no id returned"})`);
  }
  log(`\nDone — created ${missing.length} event(s).`);
}

// Guarded so the pure helpers above can be imported by tests without the
// script running a live sync on import.
if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((err) => die(err.message));
}
