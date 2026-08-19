#!/usr/bin/env node
// luma-events.mjs — pull a Luma calendar's upcoming events with zero
// dependencies and zero auth (#641).
//
// Luma publishes an ICS feed per calendar:
//   https://api.lu.ma/ics/get?entity=calendar&id=cal-XXXXXXXX
//
// No API key. The documented public API (public-api.luma.com/v1) needs a paid
// Luma Plus subscription and 400s without one; the ICS feed needs nothing and
// returns the whole calendar in a single GET. RFC 5545 also doesn't rot the
// way Meetup's persisted-query hashes do.
//
// Finding a calendar id: open any event on the calendar and look for
// `"api_id":"cal-…"` in the page's __NEXT_DATA__.
//
// Usage:
//   node scripts/luma-events.mjs <cal-id> [--json] [--limit N]
//
// Output matches `meetup-events.mjs --json`, so sources are interchangeable:
//   { urlname, kind, totalCount, count, events: [...] }
//
// Exit codes: 0 ok · 1 runtime/network error · 3 feed shape changed

const ICS_ENDPOINT = "https://api.lu.ma/ics/get";

class FeedShapeChanged extends Error {}

/** RFC 5545 folds long lines; continuations begin with a space or tab. */
function unfold(text) {
  return text.replace(/\r\n/g, "\n").replace(/\n[ \t]/g, "");
}

/** RFC 5545 TEXT escaping. */
function decodeText(value) {
  return value
    .replace(/\\n/gi, "\n")
    .replace(/\\,/g, ",")
    .replace(/\;/g, ";")
    .replace(/\\\\/g, "\\");
}

/**
 * `20260825T160000Z` (UTC), `20260825T120000` with a TZID param, or a
 * date-only `20260825`. Luma emits the UTC form; the others are handled so a
 * format change doesn't silently produce garbage times.
 */
function parseIcsDate(rawValue, params) {
  const value = rawValue.trim();
  const m = /^(\d{4})(\d{2})(\d{2})(?:T(\d{2})(\d{2})(\d{2})(Z)?)?$/.exec(value);
  if (!m) throw new FeedShapeChanged(`unparseable date: ${value}`);
  const [, y, mo, d, hh = "00", mm = "00", ss = "00", z] = m;

  if (z) return new Date(Date.UTC(+y, +mo - 1, +d, +hh, +mm, +ss)).toISOString();

  const tzid = params.TZID;
  if (!tzid) {
    // Floating time — treat as the runner's local zone, which is what a
    // calendar client would do.
    return new Date(+y, +mo - 1, +d, +hh, +mm, +ss).toISOString();
  }
  // Wall clock in an named zone: find the UTC instant whose rendering in that
  // zone matches, correcting once for the offset.
  const naive = Date.UTC(+y, +mo - 1, +d, +hh, +mm, +ss);
  const offset = (() => {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone: tzid, hour12: false, year: "numeric", month: "2-digit",
      day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
    }).formatToParts(new Date(naive));
    const read = (t) => Number(parts.find((p) => p.type === t)?.value);
    return naive - Date.UTC(read("year"), read("month") - 1, read("day"),
      read("hour") % 24, read("minute"), read("second"));
  })();
  return new Date(naive + offset).toISOString();
}

function parseIcs(text) {
  const lines = unfold(text).split("\n");
  const calName = (lines.find((l) => l.startsWith("X-WR-CALNAME:")) ?? "").slice(13).trim();

  const events = [];
  let current = null;
  for (const line of lines) {
    if (line === "BEGIN:VEVENT") { current = {}; continue; }
    if (line === "END:VEVENT") { if (current) events.push(current); current = null; continue; }
    if (!current) continue;

    const colon = line.indexOf(":");
    if (colon === -1) continue;
    const [name, ...paramParts] = line.slice(0, colon).split(";");
    const params = Object.fromEntries(
      paramParts.map((p) => { const i = p.indexOf("="); return [p.slice(0, i), p.slice(i + 1)]; }),
    );
    current[name.toUpperCase()] = { value: line.slice(colon + 1), params };
  }
  return { calName, events };
}

/**
 * Luma appends the calendar name to every SUMMARY
 * ("Founder Junto Coworking - Philly Tech Entrepreneurs"). Meetup does not.
 * Left in place, the same cross-posted event is created twice on the target
 * calendar, so strip it at the source.
 */
export function stripCalendarSuffix(title, calName) {
  if (!calName) return title.trim();
  const suffix = ` - ${calName}`;
  return (title.endsWith(suffix) ? title.slice(0, -suffix.length) : title).trim();
}

function normalize(raw, calName) {
  const get = (k) => raw[k]?.value;
  const description = get("DESCRIPTION") ? decodeText(get("DESCRIPTION")) : null;
  const urlMatch = description?.match(/https:\/\/(?:luma\.com|lu\.ma)\/[A-Za-z0-9]+/);
  const uid = (get("UID") ?? "").split("@")[0];

  return {
    id: uid || null,
    title: stripCalendarSuffix(decodeText(get("SUMMARY") ?? ""), calName),
    url: urlMatch?.[0] ?? (uid ? `https://luma.com/${uid}` : null),
    // Luma marks events TENTATIVE as a matter of course; only an explicit
    // CANCELLED means the event is off. Filtering on !== CONFIRMED empties
    // the calendar.
    status: /^CANCELLED$/i.test(get("STATUS") ?? "") ? "CANCELLED" : "ACTIVE",
    dateTime: raw.DTSTART ? parseIcsDate(raw.DTSTART.value, raw.DTSTART.params) : null,
    endTime: raw.DTEND ? parseIcsDate(raw.DTEND.value, raw.DTEND.params) : null,
    isOnline: false,
    eventType: "PHYSICAL",
    going: null,
    maxTickets: null,
    venue: get("LOCATION") ? { name: null, address: decodeText(get("LOCATION")), city: null, state: null } : null,
    recurrence: null,
    description,
  };
}

export async function fetchCalendar(calendarId) {
  const url = `${ICS_ENDPOINT}?entity=calendar&id=${encodeURIComponent(calendarId)}`;
  const res = await fetch(url, { headers: { accept: "text/calendar" } });
  if (!res.ok) throw new Error(`lu.ma returned ${res.status} for ${calendarId}`);

  const text = await res.text();
  if (!text.includes("BEGIN:VCALENDAR")) {
    throw new FeedShapeChanged(`not an ICS feed for ${calendarId} — Luma changed the endpoint`);
  }

  const { calName, events } = parseIcs(text);
  const now = Date.now();
  return events
    .map((e) => normalize(e, calName))
    .filter((e) => e.status === "ACTIVE" && e.dateTime && Date.parse(e.dateTime) >= now)
    .sort((a, b) => Date.parse(a.dateTime) - Date.parse(b.dateTime));
}

async function main() {
  const argv = process.argv.slice(2);
  let calendarId = null, json = false, limit = Infinity;
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--json") json = true;
    else if (a === "--limit") limit = Number(argv[++i]);
    else if (!a.startsWith("--") && !calendarId) calendarId = a;
    else { console.error(`Unknown option: ${a}`); process.exit(1); }
  }
  if (!calendarId) {
    console.error("Usage: node scripts/luma-events.mjs <cal-id> [--json] [--limit N]");
    process.exit(1);
  }

  const events = (await fetchCalendar(calendarId)).slice(0, limit);

  if (json) {
    console.log(JSON.stringify(
      { urlname: calendarId, kind: "upcoming", totalCount: events.length, count: events.length, events },
      null, 2,
    ));
    return;
  }
  console.log(`\n${calendarId} — ${events.length} upcoming event(s)\n`);
  for (const e of events) {
    console.log(`${e.dateTime}  ${e.title}`);
    console.log(`  ${e.venue?.address ?? "TBD"}  ·  ${e.url}\n`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((err) => {
    if (err instanceof FeedShapeChanged) { console.error(`\n⚠️  ${err.message}\n`); process.exit(3); }
    console.error(`error: ${err.message}`);
    process.exit(1);
  });
}
