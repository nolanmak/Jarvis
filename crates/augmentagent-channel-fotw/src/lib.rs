//! FlyOnTheWall transcript-repo channel (#915).
//!
//! FlyOnTheWall — a local-first meeting recorder — auto-pushes every finished
//! meeting to a private git repo as `meetings/YYYY-MM-DD-{slug}-{id8}.md`.
//! This crate pulls that repo, notices new meetings, and hands each one to the
//! *existing* wiki ingest funnel as a synthetic `Email`, exactly the way
//! `augmentagent-channel-voice` does. It makes zero changes to that pipeline.
//!
//! # The four rules this crate exists to keep
//!
//! 1. **The transcript repo is read-only here.** FlyOnTheWall re-pushes a
//!    retitled meeting to the same receipt-pinned path; if this side never
//!    writes, the two can never conflict. [`sync`] therefore has no write verb
//!    in it at all, and a test greps for that.
//! 2. **Raw transcripts never enter a prompt or the wiki.** The wiki holds
//!    distilled, cited facts; the words stay in the repo clone for the ask
//!    path to Grep. [`distill`] carries the summary and the action items and
//!    nothing below `## Transcript` — the leak test in that module is the
//!    structural guarantee, in the same spirit as the calendar channel's
//!    description/location test.
//! 3. **Dedup where every channel dedups**: the `emails` table, keyed
//!    `fotw:{id}` off the frontmatter id, which is stable across re-pushes.
//! 4. **Never guess a calendar match.** [`match_event`] answers Single,
//!    Ambiguous or None; only Single attaches a roster.

pub mod calendar;
pub mod distill;
pub mod match_event;
pub mod parse;
pub mod runner;
pub mod sync;

/// `emails.platform` for everything this crate synthesizes.
pub const PLATFORM: &str = "fotw";
/// `emails.kind` for a meeting transcript.
pub const KIND: &str = "meeting_transcript";

/// The `sources:` namespace a wiki fact cites when it came from a meeting.
pub fn source_id(meeting_id: &str) -> String {
    format!("fotw:{meeting_id}")
}
