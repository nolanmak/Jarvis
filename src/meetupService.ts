// meetupService.ts — typed wrapper around scripts/meetup-events.mjs so the
// agent and the standalone CLI share ONE implementation of the reverse-
// engineered Meetup GraphQL client. See that file for endpoint/hash details.

import path from "path";
import { pathToFileURL } from "url";

export interface MeetupEvent {
  id: string;
  title: string;
  url: string;
  status: string; // ACTIVE | CANCELLED | PAST | DRAFT
  dateTime: string | null;
  endTime: string | null;
  isOnline: boolean;
  eventType: string; // PHYSICAL | ONLINE | HYBRID
  going: number | null;
  maxTickets: number | null;
  venue: { name: string; address: string; city: string; state: string } | null;
  recurrence: string | null;
  photo: string | null;
  description: string | null;
}

export interface MeetupEventsResult {
  group: { id: string } | null;
  totalCount: number;
  events: MeetupEvent[];
}

type FetchGroupEvents = (
  urlname: string,
  opts?: { kind?: "upcoming" | "past"; limit?: number; cutoff?: string }
) => Promise<MeetupEventsResult>;

// dist/meetupService.js and src/meetupService.ts are both one level under the
// repo root, so ../scripts/ resolves the same in dev (ts-node) and prod (dist).
const scriptUrl = pathToFileURL(
  path.join(__dirname, "..", "scripts", "meetup-events.mjs")
).href;

let cached: FetchGroupEvents | null = null;

async function loadFetcher(): Promise<FetchGroupEvents> {
  if (!cached) {
    const mod = (await import(scriptUrl)) as { fetchGroupEvents: FetchGroupEvents };
    cached = mod.fetchGroupEvents;
  }
  return cached;
}

/**
 * Fetch a Meetup group's events. Public groups need no auth.
 *
 * @param urlname  group slug from meetup.com/<urlname>/ (e.g. "code-coffee-philly")
 */
export async function getGroupEvents(
  urlname: string,
  opts: { kind?: "upcoming" | "past"; limit?: number; cutoff?: string } = {}
): Promise<MeetupEventsResult> {
  const fetchGroupEvents = await loadFetcher();
  return fetchGroupEvents(urlname, opts);
}
