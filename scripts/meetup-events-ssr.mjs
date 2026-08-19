#!/usr/bin/env node
// meetup-events-ssr.mjs — pull a Meetup group's events with zero dependencies
// and zero persisted-query hashes.
//
// Companion to meetup-events.mjs, which talks to Meetup's private GraphQL
// endpoint using Apollo *persisted queries*. That approach breaks every time
// Meetup ships a frontend build, because the sha256 hashes are computed at
// runtime from the query document and are not lifted from any static bundle.
//
// This script reads the same data from the server-rendered events page
// instead: Next.js embeds the hydrated Apollo cache in `__NEXT_DATA__`, and
// the group's events are already in it. No auth, no hashes, no GraphQL.
// It survives frontend releases that break the persisted-query path.
//
// Trade-off: only the events the page renders (~10-12), so it's a "what's
// coming up" source, not a full archive. That is exactly what a calendar
// mirror needs.
//
// Usage:
//   node scripts/meetup-events-ssr.mjs <group-urlname> [--json] [--limit N]
//
// Output matches `meetup-events.mjs --json`:
//   { urlname, kind, totalCount, count, events: [...] }
//
// Exit codes: 0 ok · 1 runtime/network error · 3 page shape changed

const UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36";

class PageShapeChanged extends Error {}

async function fetchApolloState(urlname) {
  const url = `https://www.meetup.com/${encodeURIComponent(urlname)}/events/`;
  const res = await fetch(url, { headers: { "user-agent": UA } });
  if (res.status === 404) throw new PageShapeChanged(`group not found: ${urlname}`);
  if (!res.ok) throw new Error(`meetup.com returned ${res.status} for ${urlname}`);

  const html = await res.text();
  const match = html.match(/<script id="__NEXT_DATA__"[^>]*>([\s\S]*?)<\/script>/);
  if (!match) {
    throw new PageShapeChanged(
      "no __NEXT_DATA__ on the events page — Meetup changed its rendering",
    );
  }

  let state;
  try {
    state = JSON.parse(match[1])?.props?.pageProps?.__APOLLO_STATE__;
  } catch (err) {
    throw new PageShapeChanged(`__NEXT_DATA__ did not parse: ${err.message}`);
  }
  if (!state) throw new PageShapeChanged("no __APOLLO_STATE__ in __NEXT_DATA__");
  return state;
}

/** Apollo stores related entities as {__ref}; follow one hop. */
function deref(state, value) {
  if (value && typeof value === "object" && typeof value.__ref === "string") {
    return state[value.__ref] ?? null;
  }
  return value ?? null;
}

/** Same shape as meetup-events.mjs `normalize`, so consumers are interchangeable. */
function normalize(state, node) {
  const venue = deref(state, node.venue);
  const going = deref(state, node.going);
  const series = deref(state, node.series);
  return {
    id: node.id,
    title: node.title,
    url: node.eventUrl,
    status: node.status,
    dateTime: node.dateTime,
    endTime: node.endTime,
    isOnline: node.isOnline,
    eventType: node.eventType,
    going: going?.totalCount ?? null,
    maxTickets: node.maxTickets ?? null,
    venue: venue
      ? { name: venue.name, address: venue.address, city: venue.city, state: venue.state }
      : null,
    recurrence: series?.description ?? null,
    description: node.description ?? null,
  };
}

function parseArgs(argv) {
  const args = { json: false, limit: Infinity };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--json") args.json = true;
    else if (a === "--limit") args.limit = Number(argv[++i]);
    else if (!a.startsWith("--") && !args.urlname) args.urlname = a;
    else {
      console.error(`Unknown option: ${a}`);
      process.exit(1);
    }
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.urlname) {
    console.error(
      "Usage: node scripts/meetup-events-ssr.mjs <group-urlname> [--json] [--limit N]",
    );
    process.exit(1);
  }

  const state = await fetchApolloState(args.urlname);
  const now = Date.now();

  const events = Object.entries(state)
    .filter(([key]) => key.startsWith("Event:"))
    .map(([, node]) => normalize(state, node))
    // Upcoming and not cancelled — a mirror must never publish a dead event.
    .filter((e) => e.status === "ACTIVE" && e.dateTime && Date.parse(e.dateTime) >= now)
    .sort((a, b) => Date.parse(a.dateTime) - Date.parse(b.dateTime))
    .slice(0, args.limit);

  if (args.json) {
    console.log(
      JSON.stringify(
        { urlname: args.urlname, kind: "upcoming", totalCount: events.length, count: events.length, events },
        null,
        2,
      ),
    );
    return;
  }

  console.log(`\n${args.urlname} — ${events.length} upcoming event(s)\n`);
  for (const e of events) {
    console.log(`${e.dateTime}  ${e.title}`);
    console.log(`  ${e.venue?.name ?? (e.isOnline ? "Online" : "TBD")}  ·  ${e.url}\n`);
  }
}

main().catch((err) => {
  if (err instanceof PageShapeChanged) {
    console.error(`\n⚠️  ${err.message}\n`);
    process.exit(3);
  }
  console.error(`error: ${err.message}`);
  process.exit(1);
});
