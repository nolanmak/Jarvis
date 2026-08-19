// #641 — cross-source dedupe for the Wix calendar mirror.
//
// The hazard this pins: Philly Tech Entrepreneurs cross-posts to Meetup and
// Luma, and Luma appends its calendar name to every SUMMARY. Matching titles
// exactly would create the same event twice on a live public calendar.

const test = require("node:test");
const assert = require("node:assert");
const path = require("node:path");
const { pathToFileURL } = require("node:url");

const syncUrl = pathToFileURL(path.join(__dirname, "..", "scripts", "wix-events-sync.mjs"));
const lumaUrl = pathToFileURL(path.join(__dirname, "..", "scripts", "luma-events.mjs"));

const at = (iso, title) => ({ title, startDate: iso, label: `${iso} ${title}` });
const START = "2026-08-25T16:00:00.000Z";

test("Luma's calendar suffix is stripped at the source", async () => {
  const { stripCalendarSuffix } = await import(lumaUrl);
  assert.equal(
    stripCalendarSuffix("Founder Junto Coworking - Philly Tech Entrepreneurs", "Philly Tech Entrepreneurs"),
    "Founder Junto Coworking",
  );
  // Only a trailing suffix, and only when the calendar name is known.
  assert.equal(stripCalendarSuffix("Philly Tech Entrepreneurs - Demo Day", "Philly Tech Entrepreneurs"),
    "Philly Tech Entrepreneurs - Demo Day");
  assert.equal(stripCalendarSuffix("Founder Junto Coworking", ""), "Founder Junto Coworking");
});

test("titles normalise past case and punctuation drift", async () => {
  const { normalizeTitle } = await import(syncUrl);
  assert.equal(normalizeTitle("Coffee&Code Meetup"), normalizeTitle("coffee & code   meetup"));
  assert.equal(normalizeTitle("Founder Junto — Coworking"), normalizeTitle("Founder Junto - Coworking"));
});

test("a cross-posted event collapses to one", async () => {
  const { dedupeAcrossSources } = await import(syncUrl);
  const kept = dedupeAcrossSources([
    at(START, "Founder Junto Coworking"),
    at(START, "Founder Junto Coworking"),
  ]);
  assert.equal(kept.length, 1);
});

test("collapses even if the calendar suffix survives the source strip", async () => {
  const { dedupeAcrossSources } = await import(syncUrl);
  const kept = dedupeAcrossSources([
    at(START, "Founder Junto Coworking"),
    at(START, "Founder Junto Coworking - Philly Tech Entrepreneurs"),
  ]);
  assert.equal(kept.length, 1, "suffixed cross-post must not create a second event");
});

test("distinct events at the same time are both kept", async () => {
  const { dedupeAcrossSources } = await import(syncUrl);
  const kept = dedupeAcrossSources([
    at(START, "Founder Junto Coworking"),
    at(START, "AI Philly Hack Night"),
  ]);
  assert.equal(kept.length, 2);
});

test("the same title at a different time is a different event", async () => {
  const { dedupeAcrossSources } = await import(syncUrl);
  const kept = dedupeAcrossSources([
    at(START, "Founder Junto Coworking"),
    at("2026-09-01T16:00:00.000Z", "Founder Junto Coworking"),
  ]);
  assert.equal(kept.length, 2, "a recurring series must not collapse to a single event");
});

test("a Wix event carrying the calendar suffix still matches a Meetup plan", async () => {
  // Whichever source reached Wix first owns the stored title. Comparing
  // strictly here would recreate the event from the other source.
  const { isSameListing } = await import(syncUrl);
  assert.equal(
    isSameListing(
      at(START, "Founder Junto Coworking"),
      at(START, "Founder Junto Coworking - Philly Tech Entrepreneurs"),
    ),
    true,
  );
});

test("a minute of clock skew still counts as the same event", async () => {
  const { isSameListing } = await import(syncUrl);
  assert.equal(isSameListing(at(START, "X"), at("2026-08-25T16:00:30.000Z", "X")), true);
  assert.equal(isSameListing(at(START, "X"), at("2026-08-25T16:05:00.000Z", "X")), false);
});
