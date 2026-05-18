#!/usr/bin/env node
// meetup-events.mjs — pull a Meetup group's events with zero dependencies.
//
// Reverse-engineered from www.meetup.com's private GraphQL endpoint
// (`POST /gql2`). It uses Apollo *persisted queries*: requests carry an
// operationName + variables + a sha256 hash instead of a raw query, and the
// server resolves the hash on its own. Public group events need NO auth —
// no cookies, no API key, no browser.
//
// Hashes were captured via Claude Intercept (`/intercept`). They are tied to
// Meetup's deployed frontend bundle and will change when Meetup ships a new
// build. When that happens the endpoint returns a `PersistedQueryNotFound`
// error — re-run /intercept, browse the group's events page, and lift the new
// `sha256Hash` for the relevant operation into MEETUP_QUERIES below.
//
// Usage:
//   node scripts/meetup-events.mjs <group-urlname> [options]
//
//   <group-urlname>   e.g. code-coffee-philly (from meetup.com/<urlname>/)
//
// Options:
//   --past            Past events instead of upcoming
//   --json            Emit normalized JSON array (default: human table)
//   --raw             Emit the raw GraphQL event nodes as JSON
//   --limit N         Stop after N events (default: all)
//   --since ISO       Override the upcoming cutoff (default: now)
//   --before ISO      Override the past cutoff (default: now)
//
// Examples:
//   node scripts/meetup-events.mjs code-coffee-philly
//   node scripts/meetup-events.mjs code-coffee-philly --json --limit 5
//   node scripts/meetup-events.mjs code-coffee-philly --past --json
//
// Exit codes: 0 ok · 1 runtime/network error · 2 persisted-query hash stale

const ENDPOINT = "https://www.meetup.com/gql2";

const MEETUP_QUERIES = {
  upcoming: {
    operationName: "getUpcomingGroupEvents",
    sha256Hash:
      "066e3709c68718d5ce9dd909e979ac70f99835fb3722cef77756ded808d5ca08",
    cutoffVar: "afterDateTime",
  },
  past: {
    operationName: "getPastGroupEvents",
    sha256Hash:
      "321388b1e4a11b17a57efe3ae7a90abfecbc703a4f4e99519772294924c21351",
    cutoffVar: "beforeDateTime",
  },
};

class PersistedQueryStale extends Error {}

async function gql(operationName, variables, sha256Hash) {
  const res = await fetch(ENDPOINT, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      // Meetup keys persisted-query resolution off the client name.
      "apollographql-client-name": "nextjs-web",
      "user-agent":
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) " +
        "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36",
      origin: "https://www.meetup.com",
    },
    body: JSON.stringify({
      operationName,
      variables,
      extensions: { persistedQuery: { version: 1, sha256Hash } },
    }),
  });

  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    throw new Error(`Non-JSON response (HTTP ${res.status}): ${text.slice(0, 200)}`);
  }

  if (json.errors?.length) {
    const codes = json.errors.map((e) => e?.extensions?.code).join(",");
    if (/PersistedQueryNotFound/i.test(codes + JSON.stringify(json.errors))) {
      throw new PersistedQueryStale(
        `Meetup rejected the persisted-query hash for "${operationName}". ` +
          `The frontend bundle changed — refresh the hash via /intercept.`
      );
    }
    throw new Error(`GraphQL error: ${JSON.stringify(json.errors).slice(0, 300)}`);
  }
  return json.data;
}

function normalize(node) {
  return {
    id: node.id,
    title: node.title,
    url: node.eventUrl,
    status: node.status, // ACTIVE | CANCELLED | PAST | DRAFT
    dateTime: node.dateTime,
    endTime: node.endTime,
    isOnline: node.isOnline,
    eventType: node.eventType, // PHYSICAL | ONLINE | HYBRID
    going: node.going?.totalCount ?? null,
    maxTickets: node.maxTickets ?? null,
    venue: node.venue
      ? {
          name: node.venue.name,
          address: node.venue.address,
          city: node.venue.city,
          state: node.venue.state,
        }
      : null,
    recurrence: node.series?.description ?? null,
    photo: node.featuredEventPhoto?.highResUrl ?? node.displayPhoto?.highResUrl ?? null,
    description: node.description ?? null,
  };
}

/**
 * Fetch a group's events, auto-paginating until exhausted or `limit` reached.
 *
 * @param {string} urlname  group slug, e.g. "code-coffee-philly"
 * @param {object} [opts]
 * @param {"upcoming"|"past"} [opts.kind="upcoming"]
 * @param {number} [opts.limit=Infinity]   max events to return
 * @param {string} [opts.cutoff]           ISO datetime; defaults to now
 * @returns {Promise<{group:object, totalCount:number, events:object[]}>}
 */
export async function fetchGroupEvents(urlname, opts = {}) {
  const kind = opts.kind === "past" ? "past" : "upcoming";
  const q = MEETUP_QUERIES[kind];
  const limit = opts.limit ?? Infinity;
  const cutoff = opts.cutoff ?? new Date().toISOString();

  const events = [];
  let after;
  let totalCount = 0;
  let group = null;

  while (events.length < limit) {
    const variables = { urlname, [q.cutoffVar]: cutoff };
    if (after) variables.after = after;

    const data = await gql(q.operationName, variables, q.sha256Hash);
    const g = data?.groupByUrlname;
    if (!g) throw new Error(`Group "${urlname}" not found or has no events.`);

    if (!group) group = { id: g.id };
    const conn = g.events;
    totalCount = conn.totalCount;

    for (const edge of conn.edges) {
      events.push(normalize(edge.node));
      if (events.length >= limit) break;
    }

    if (!conn.pageInfo.hasNextPage || events.length >= limit) break;
    after = conn.pageInfo.endCursor;
  }

  return { group, totalCount, events };
}

// ---- CLI ----------------------------------------------------------------

function parseArgs(argv) {
  const args = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--past") args.past = true;
    else if (a === "--json") args.json = true;
    else if (a === "--raw") args.raw = true;
    else if (a === "--limit") args.limit = Number(argv[++i]);
    else if (a === "--since") args.since = argv[++i];
    else if (a === "--before") args.before = argv[++i];
    else if (a.startsWith("--")) {
      console.error(`Unknown option: ${a}`);
      process.exit(1);
    } else args._.push(a);
  }
  return args;
}

function fmtRow(e) {
  const when = e.dateTime
    ? new Date(e.dateTime).toLocaleString("en-US", {
        weekday: "short",
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
      })
    : "—";
  const where = e.isOnline ? "Online" : e.venue?.name ?? "—";
  const tag = e.status !== "ACTIVE" ? ` [${e.status}]` : "";
  return `  ${when.padEnd(24)}  ${String(e.going ?? "").padStart(3)} going  ${e.title}${tag}\n` +
    `  ${" ".repeat(24)}  ${where} · ${e.url}`;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const urlname = args._[0];
  if (!urlname) {
    console.error("Usage: node scripts/meetup-events.mjs <group-urlname> [--past] [--json] [--raw] [--limit N]");
    process.exit(1);
  }

  try {
    const kind = args.past ? "past" : "upcoming";
    const { totalCount, events } = await fetchGroupEvents(urlname, {
      kind,
      limit: Number.isFinite(args.limit) ? args.limit : Infinity,
      cutoff: args.since || args.before || undefined,
    });

    if (args.raw || args.json) {
      console.log(JSON.stringify({ urlname, kind, totalCount, count: events.length, events }, null, 2));
      return;
    }

    console.log(`\n${urlname} — ${events.length} of ${totalCount} ${kind} event(s)\n`);
    for (const e of events) console.log(fmtRow(e) + "\n");
  } catch (err) {
    if (err instanceof PersistedQueryStale) {
      console.error(`\n⚠️  ${err.message}\n`);
      process.exit(2);
    }
    console.error(`error: ${err.message}`);
    process.exit(1);
  }
}

// Run as CLI only when invoked directly, not when imported.
import { fileURLToPath } from "node:url";
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main();
}
