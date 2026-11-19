# Heartbeat

The heartbeat is a **daily safety net** (#1317, reworked in #2285). Once a day
it looks back over the last two weeks for things that slipped through the
cracks, and posts at most a few of them on one notice card. It does not
repeat what just arrived: real-time triage already alerts you to new
messages, and nothing younger than two days is ever a heartbeat item.

Code collects the candidates (bounded SQL over the store, plus cheap sender
filters). The model only judges that short list against your checklist and
words each item. No keyword match decides that something matters.

Hermes Agent's guidance shaped this: collect candidates with code, let the
model judge. Issue #2285 has the audit that motivated it.

## What changed from the 30-minute heartbeat

The original heartbeat (#1317) ran every 30 minutes and showed the model only
the inbound messages since its last run plus the number of waiting approval
cards. In practice it spent its daily cap on "approval cards are piling up"
and on repeats of real-time triage flags, and it never looked back far enough
to catch anything that slipped. All of that is **removed**:

- There is no 30-minute pass. `AUGMENTAGENT_HEARTBEAT_INTERVAL` is ignored
  (a leftover setting only produces a warning).
- The approval-card count is no longer in the prompt. There is no
  approval-backlog notice of any kind.
- The "inbound since the last heartbeat" snapshot is gone.

## Setup

1. Write a checklist at `<wiki>/HEARTBEAT.md`. It is your standing guidance on
   what matters when the model judges candidates. Short, concrete lines work
   best:

   ```markdown
   # Heartbeat
   - Tell me about approvals, acceptances and anything about money.
   - Investors, customers and family always deserve an answer.
   - Ignore anything about free trials, renewals or promotions.
   ```

   Don't put real-time checks or approval-queue reminders here; the sweep
   runs once a day and never sees the approval queue. A file that is missing,
   or holds only headings, comments, blank lines or empty list items, is
   skipped **without a model call**. You can leave the feature enabled and
   pause it by emptying the file.

2. Try it without delivering anything:

   ```bash
   augmentagent --wiki-dir ./wiki heartbeat run-once --dry-run --force
   ```

3. Enable it in `.env` and restart the daemon:

   ```bash
   AUGMENTAGENT_HEARTBEAT_ENABLED=1
   AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS=08:00-22:00
   AUGMENTAGENT_HEARTBEAT_TZ=America/New_York
   # optional: AUGMENTAGENT_HEARTBEAT_SWEEP_AT=09:00  AUGMENTAGENT_HEARTBEAT_MAX_ITEMS=5
   ```

The daemon (`serve`) starts the loop only when it is enabled, `--wiki-dir` is
set and it is not a dry run. At startup it logs `heartbeat disabled: <why>`
otherwise.

## Configuration

| Variable | Default | Notes |
|---|---|---|
| `AUGMENTAGENT_HEARTBEAT_ENABLED` | off | `1`, `true`, `yes` or `on` |
| `AUGMENTAGENT_HEARTBEAT_SWEEP_AT` | `09:00` | Local time of the daily sweep, `HH:MM` in the heartbeat zone |
| `AUGMENTAGENT_HEARTBEAT_MAX_ITEMS` | `5` | Items on one sweep notice at most |
| `AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS` | all day | `HH:MM-HH:MM`, start inclusive, end exclusive, `24:00` allowed; `22:00-06:00` crosses midnight; equal start and end means never. A sweep that falls due outside the window waits for it to open. |
| `AUGMENTAGENT_HEARTBEAT_TZ` | host local | IANA zone for the sweep time, the window and the time shown to the model |
| `AUGMENTAGENT_HEARTBEAT_DAILY_CAP` | `6` | Maximum notices delivered per rolling 24h (a backstop for forced runs) |
| `AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS` | `300` | Wall-clock budget for one model call |
| `AUGMENTAGENT_HEARTBEAT_INTERVAL` | — | **Removed.** Ignored with a warning. |

A value that doesn't parse falls back to its default and produces a warning.
The warning is logged at startup and shown by `heartbeat status`. An invalid
window means "all day", not "never".

## Cadence

The loop checks every minute whether the sweep is due. It is due once the
day's sweep time has passed and no run since then has settled it. A run
settles the day when it ends silent, sent, or skipped by a gate other than
`busy`. A run that ends in error does not, so a failed sweep is retried an
hour later, not tomorrow. Because this is measured from the runs stored in
the database, restarts neither double-fire nor reset the clock, and a
long outage produces one catch-up sweep. A sweep that falls due outside
active hours is not recorded; it runs when the window opens.

## Candidates

All collectors look at messages 2 to 14 days old. Each is bounded, and each
leaves out anything already offered, surfaced or marked done.

1. **Unanswered people** (up to 15 conversations). A direct message (`dm` or
   email) where the person has not written again in the last two days, and
   the owner has not replied since on any channel. Gmail counts only when
   triage flagged it or drafted a reply; mail triage skipped or sent to the
   digest is never "a person waiting", and mailing-list mail is left out.
   The reply check is the `ReplyEvidence` trait. Its default implementation
   (`StoreReplyEvidence`) looks for an owner message later in the same
   conversation (`message_index.from_me`), a send in the same Gmail thread
   (`outbound_thread_log`, `self_sent_messages`), or an owner message to the
   same person on another channel (`message_people`). The cross-channel reply
   detection from #2284 plugs in through
   `HeartbeatRunner::with_reply_evidence`.
2. **Automated mail triage skipped** (up to 60, one per sender). Gmail that
   triage skipped or never triaged. These are left out by sender and
   category, never by importance:
   - mailing-list mail (a `List-Unsubscribe`/`List-Id` header);
   - bulk senders: 6 or more mails from the address, or 20 or more from the
     domain, in 30 days;
   - developer-tool and deploy senders (GitHub, Vercel, Netlify, CI);
   - marketing sending domains (`promotion.`, `news.`, `marketing.` and so on);
   - trial-ending, renewal and promo reminders, which you asked never to see.

   The model then picks only mail that records an outcome you would want to
   know: an acceptance or approval, money moved or owed, an account or
   security change, or an invitation still awaiting a response.
3. **Snoozed items** whose snooze ran out come back on their own, ahead of
   new candidates, without a model call.

Not built yet (seams, each with a follow-up issue): open promises in your
sent messages, calendar invites never answered, relationship gaps with
people marked important, and money (unpaid invoices, failed payments).

## What one sweep does

Each due run goes through these gates in order. Everything before step 6
costs nothing:

1. **Lease.** If another process holds the run (for example the daemon while
   you use `run-once`), the run is skipped with `busy`.
2. **Checklist.** Missing or effectively empty skips with `empty-checklist`.
3. **Cap.** Once the daily notice cap is reached, runs are skipped with `cap`.
4. **Candidates.** Nothing to judge skips with `no-candidates`. Only expired
   snoozes posts them without a model call.
5. **Record.** A `running` row is written *before* the model call, so a crash
   can't cause a double-fire. A row left over from a dead process becomes
   `error/interrupted` on the next run.
6. **Model call.** A fresh session on the fast tier, with read-only wiki tools
   (`Read`, `Grep`, `Glob`), under the configured timeout. The prompt contains
   the local time, the last run and last notice, how many items it may pick,
   the numbered candidates (source, channel, age, sender, subject and a short
   snippet), and the checklist. Candidate text is marked as untrusted data.
7. **Decision.** See the next section.
8. **Delivery.** One card lists the picked items, one line each with the
   item's age and short ref, for example
   `• Example Cloud approved your credits; claim them (10d, 3fa2c1)`. Only
   whole lines that fit the card's 500 characters are posted; five
   full-length notes always fit, and an item that doesn't fit is left for
   the next sweep.
9. **Dedup.** After the card posts, every picked item is recorded as
   `surfaced` and every other candidate as `offered`, in `heartbeat_items`.
   Neither is offered again. If posting fails, the run is `error/delivery`,
   nothing is recorded, and the sweep is retried an hour later. Dry runs
   record nothing.

After three errors in a row, the heartbeat posts **one** "Heartbeat failing"
card. It posts nothing more until a run succeeds. Runs are kept for 30 days;
item bookkeeping for 60 (snoozed items are kept until a sweep resurfaces them).

## Decision contract

The model must reply with exactly one JSON object:

```json
{"notify": false}
{"notify": true, "items": [{"id": "c3", "note": "Example Cloud approved your credits 10 days ago; claim them."}]}
```

- `id` must be one of the candidate ids in the prompt. Unknown or repeated
  ids and empty notes are dropped. Notes are cut to 75 characters, and only
  the first `MAX_ITEMS` (less any expired snoozes) are kept.
- `notify: true` with no valid item is `error/invalid-output` and is never
  delivered.
- JSON inside a code fence, or after other prose, is accepted. If there are
  several objects, the last one wins.
- `HEARTBEAT_OK` at the start or end of the reply is accepted as a synonym
  for silence (OpenClaw compatibility). It is rejected when more than 300
  characters of other text come with it.
- Anything else, including free prose, is recorded as `error/invalid-output`
  and is never delivered.

With proactive outreach on (#1833 D16, `AUGMENTAGENT_HEARTBEAT_OUTREACH=on`
plus the autonomy switch), the same object may carry an `"outreach"` list of
messages to full-tier trusted contacts, which go through the autonomy gate
(`crates/augmentagent-autonomy/src/outreach.rs`). Outreach now runs only on
days the sweep calls the model.

## Done and snooze

Every item surfaces once and then stays quiet. To act on one, use the short
ref printed on the card:

```bash
augmentagent heartbeat items [--json]          # what was surfaced, and its state
augmentagent heartbeat done 3fa2c1             # handled; never shown again
augmentagent heartbeat snooze 3fa2c1 --for 3d  # show it again in 3 days
```

Card buttons for Done and Snooze are a follow-up.

## CLI

```bash
augmentagent --wiki-dir ./wiki heartbeat run-once [--force] [--dry-run] [--json]
augmentagent heartbeat status [--json] [--check]
augmentagent heartbeat items [--limit N] [--json]
augmentagent heartbeat done <ref|key>
augmentagent heartbeat snooze <ref|key> [--for 3d]
```

- `run-once` goes through the same gates as the loop.
  - `--force` ignores the schedule and active hours. The checklist, cap,
    item dedup and lease still apply.
  - `--dry-run` calls the model but delivers nothing and records nothing.
- `status` shows the configuration, whether the checklist is present, any
  warnings, the last 10 runs and the last 10 surfaced items.
- `status --check` is a liveness probe. It exits `1` when the heartbeat is
  enabled, the active window is open, and no sweep has been attempted for 26
  hours. For example, from cron:

  ```bash
  0 * * * * cd ~/AugmentAgent && ./target/release/augmentagent --wiki-dir ./wiki heartbeat status --check >/dev/null || curl -fsS https://hc-ping.com/<uuid>/fail
  ```

## Cost

One fast-tier call a day at most (plus an hourly retry after an error).
Days with no candidates, an empty checklist, or a reached cap cost nothing.
Each run starts a fresh session instead of carrying history.
