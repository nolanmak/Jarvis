# Calendar create approval verification (#1436)

Calendar creation uses Composio's boolean `send_updates`, converts the CLI's
RFC3339 start to a naive UTC datetime with `timezone: "UTC"`, and confirms
creation only when the provider returns a nonempty event ID. Composio tool
failures inside HTTP 200 responses are errors for create, list and get.

The approval acknowledgment includes the confirmed event ID and optional link.
A failed creation leaves an error on the action and displays its reason in
Discord. A missing handle, malformed response, transport failure or upstream
5xx is **unconfirmed**: check the calendar before proposing another event.
Create requests are not automatically retried; read requests retain their
bounded retry policy. HTTP requests have a 30-second timeout.

## Automated regression tests

Run from the implementation worktree with a target directory on a volume with
build capacity:

```sh
export CARGO_TARGET_DIR=/path/to/private/calendar-qa-target
cargo test -p augmentagent-channel-calendar -p augmentagent-approval-discord --lib
cargo test -p augmentagent-cli --bin augmentagent calendar_approval_tests -- --test-threads=1
cargo check --workspace
cargo build -p augmentagent-cli --bin augmentagent
```

The client tests capture the actual HTTP request and exercise unsuccessful
Composio envelopes, malformed/missing handles, timestamp offsets (including a
UTC date rollover), write retry prevention and bounded diagnostics. The CLI
tests execute the real approver against isolated SQLite and local HTTP. The
Discord tests execute the production approval-delivery function using real
Serenity serialization against a local HTTP server and assert followup/delete
requests. These tests do not require provider credentials.

## Owner-authorized live approver test

This opt-in test sends real invitations. Configure only accounts owned by the
operator: organizer first, followed by exactly two attendee accounts. Each
entity must have its own active Calendar/Gmail connection. The organizer must
support Meet creation. Do not commit the configuration or report.

Provide `COMPOSIO_API_KEY` through the private environment and set:

```sh
export JARVIS_CALENDAR_QA_ACCOUNTS_JSON='[
  {"email":"organizer@example.test","entity_id":"organizer-entity"},
  {"email":"owner-one@example.test","entity_id":"attendee-one-entity"},
  {"email":"owner-two@example.test","entity_id":"attendee-two-entity"}
]'
# Use a private existing directory and an absolute report path.
export JARVIS_CALENDAR_QA_REPORT=/private/qa/calendar-report.json
cargo test -p augmentagent-cli --bin augmentagent \
  calendar_live_tests::live_calendar_self_invite_and_cleanup \
  -- --ignored --exact --nocapture --test-threads=1
```

The test creates an isolated SQLite proposal, invokes the real Rust approver,
and verifies one unique event on all three calendars: ID, organizer, attendees,
start/end instants and a Meet link. It polls both attendee inboxes for up to
120 seconds. It attempts deletion even when readback verification fails, then
checks all three calendars for absence/cancellation within 120 seconds. Only
the recorded diagnostic event is deleted. The report records the event ID
before verification so an interrupted run can be cleaned up explicitly.

The test uses a unique `[TEST #1436]` title and a five-minute meeting one day
in the future. Invitation/cancellation mail is left intact. It does not write
the live daemon's SQLite database or restart/deploy the daemon.

**This is live approver/provider QA, not a live Discord click.** The issue's
final end-to-end gate additionally requires the built version to handle a real
CLI proposal and owner click through Discord. Record that version's commit,
action/event IDs, acknowledgment and independent calendar/inbox results. Use
only owned accounts and clean up the recorded test IDs. A disappearing card,
HTTP 200, or mocked transport test alone does not satisfy that gate.
