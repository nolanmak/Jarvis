# Actionable owner alerts

Owner alerts are independent of reply approvals and the six-hour approval
carousel. The daemon evaluates their persistent schedule every 15 seconds.
Critical alerts post immediately with the configured owner's actual Discord
mention. High alerts also post immediately; an unacknowledged item gets one
follow-up after ten minutes. Unresolved meeting preparation gets reminders at
60 and 15 minutes before the meeting, including after acknowledgment. Overlapping
triggers coalesce; the default cap is six notices per alert, including reschedules.

Email triage supplies a concrete request and verbatim supporting evidence. The
classifier requires supported deadline/consequence evidence or an explicit owner
sender priority. A subject saying “URGENT” alone is insufficient. Calendar links
require the same connected account, sender participation, and evidence in the
email linking the task to that meeting. An inferred preparation cutoff is labeled;
an unknown deadline stays unknown. Calendar descriptions and locations are not
included in the context. Private/confidential events are excluded, and a visibility
change removes cached context and stops linked reminders. Prior processed reply/flag emails from the last 14 days
are reconsidered in batches of ten each minute, prioritizing participant matches
to meetings in the next day. Changed future calendar evidence reopens assessment
only for matching participants in the same account. Valid triage without an alert
is assessed once. Failed assessments retry after one and then five minutes, with
three total attempts; persistent failure records prevent starvation and repeated
model calls across restarts. New relevant meeting evidence or a changed sender
priority starts a fresh retry cycle. `--no-email true` disables this backfill;
existing Discord alert schedules remain active.

## Configure

Use the existing `DISCORD_BOT_TOKEN`, `DISCORD_CHANNEL_ID`, and
`DISCORD_ALLOWED_USER_ID` settings and include Discord in the configured approval
surfaces. Set `AUGMENTAGENT_ALERT_TIMEZONE=America/New_York` (or your IANA timezone)
so absolute deadlines match your location. The default is UTC. The bot needs View
Channel, Send Messages, and Embed Links. Posts restrict allowed mentions to the
owner; they do not ping roles or everyone.

```sh
augmentagent imessage alerts priority colleague@example.test --urgency high
augmentagent imessage alerts policy --followup-seconds 600 \
  --prepare-first-seconds 3600 --prepare-last-seconds 900 --max-notices 6
```

Priorities accept a bare email, wiki person slug (`matt` or `person:matt`), or
exact domain (`example.test`, `@example.test`, `domain:example.test`). Levels are
`high`, `critical`, or `routine` (mute new proactive classification). Domains do
not include subdomains. An exact email wins over a person, which wins over a
domain; a mute wins equal-specificity ties. Person selectors resolve every email
identity on the current wiki page, including identities added later. Pass the
same `--wiki-dir` used by the daemon when saving a person selector. Changing a priority reconsiders prior eligible
messages. Already-created tasks retain their lifecycle; resolve unwanted ones.
The schedule is stored in SQLite and survives restart. A one-shot
`augmentagent imessage alerts dispatch` evaluates the same Discord schedule.
The command lives under `imessage alerts` because both transports share the
owner-alert record and controls.

Text escalation is separately opted in as described in [IMESSAGE.md](IMESSAGE.md).
Critical texts are immediately eligible; high texts wait 600 seconds by default
(`AUGMENTAGENT_ALERT_TEXT_DELAY_SECS`). A Discord failure accelerates text
eligibility. With texts disabled or the Mac unavailable, Discord reminders remain
active. Sent, failed, unknown, expired, and cancelled text outcomes remain distinct.

## Optional email gate and High digest

Existing classification and sending defaults remain unchanged. Set
`AUGMENTAGENT_ALERT_EMAIL_GATES=1` in the daemon environment to enable the additive
email gate and the default digest schedule, `09:00,17:00`. This does not enable
text delivery: the existing verified owner text policy and
`AUGMENTAGENT_IMESSAGE_SEND_ENABLED` opt-ins are still required.

```sh
augmentagent --wiki-dir ./wiki imessage alerts priority matt high
augmentagent imessage alerts priority example.test --urgency high
augmentagent imessage alerts priority newsletter@example.test routine
```

With the gate enabled, priority senders qualify on arrival without a request,
consequence, or deadline quote. They do not wait for model triage. A person with
an explicit `relationship: active-collaborator` or `relationship: close-contact`
line also qualifies as High on a concrete request alone. Affirmative prose such
as “Active collaborator on agent projects.” or “Close contact from the community.”
is supported; hypothetical, former, and negated descriptions do not qualify.
The same request from an unknown person follows the existing evidence rules.
Ambiguous identities shared by multiple people do not infer a relationship.
The exact request and relationship source line are retained in alert evidence.
The deterministic request recognizer accepts direct forms such as “can you send”,
“please review”, and “need you to confirm”; it does not treat every question as a
request. Unsupported phrasings continue through ordinary model assessment.

`AUGMENTAGENT_ALERT_HIGH_DIGEST=09:00,17:00` can independently enable/customize
the digest; an explicitly empty value disables it, including when the email gate
is enabled. Times use `AUGMENTAGENT_ALERT_TIMEZONE` (UTC if unset). At each local
minute, one durable batch contains up to the oldest 20 open, unexpired,
unsnoozed High alerts not individually sent, in flight, or of unknown delivery.
Acknowledged, resolved, Critical and Routine items are excluded. An empty batch
sends nothing. The body is refreshed at claim, so acknowledgment or an individual
text arriving after scheduling removes that item. A batch expires five minutes
after its slot; restart never resends a claimed/sent/unknown batch. A missed slot
is not replayed hours later. DST's repeated local minute produces one batch.
The existing sender's five-second dispatch deadline also applies to digests.

## New-DM alerts

Off for every platform until you opt one in with
`AUGMENTAGENT_ALERT_DM_POLICY=imessage=first-contact`. Each entry
is exactly `<platform>=<off|priority|first-contact|all>`, comma separated and
matched verbatim — no surrounding spaces, no other spelling or case. An unknown
mode, a missing `=`, or a platform named twice leaves *that* platform off and
never affects another one; an absent variable alerts on nothing anywhere.

Only an inbound 1:1 direct message can qualify — never a group, channel, bot,
service or short-code sender, or the owner's own message. Two classes qualify: a
**first contact** (no prior stored inbound message in the conversation) and a
**priority contact** (the handle resolves, through the wiki identity index on
the exact handle only, to a person on the `imessage alerts priority` list, muted
`routine` selectors excluded). `first-contact` and `priority` enable only their
own class, `all` both. A conversation first seen with only messages at or
before the newest iMessage already stored is a backfill, not a DM.

Only the iMessage poll evaluates this policy today; the other messaging
channels are wired in follow-up slices of #1643, so naming another platform has
no effect until its channel calls the shared evaluator.

A qualifying message creates one **High** owner alert per platform conversation
per rolling hour; the claim lives in SQLite, so restarting mid-hour does not
produce a second one. From there it follows the owner-alert paths above — the
Discord schedule, and text escalation only behind the existing `imessage alerts
configure --enabled` opt-in. Nothing reaches the DM's sender.

## Offline calibration

No provider calls, database mutations, or sends are needed. On the machine that
holds the live database, run the exporter with read-only SQLite (enforced by URI
`mode=ro` and `query_only=ON`), piping its private output to the development
machine. Restrict the destination file to mode 0600. Then run locally:

```sh
python3 scripts/export-owner-alert-calibration.py --db data.db --wiki wiki --days 30 > snapshot.jsonl
augmentagent imessage alerts calibrate --input snapshot.jsonl
```

The CLI handles calibration before opening its normal database. The JSON report
contains aggregate counts/rates only, never subjects, addresses, or body text.
It compares retained triaged emails in the last 30 days: the baseline is recorded
High/Critical alert outcomes; the revised result applies the exact deterministic
priority/relationship rules while holding the recorded legacy result fixed.
It uses current wiki relationships/priorities, so it is a counterfactual rather
than a reconstruction of historical wiki state. It does not replay model calls
or measure phone delivery, and cannot recover emails no longer retained. Delete
the private input after use. Record only the report on the issue; the owner can
then enable `AUGMENTAGENT_ALERT_EMAIL_GATES` and/or choose digest times.

## Controls and recovery

Seen is not done: **Mark as seen** only records that you read the alert, while
**Mark done / stop reminders** is the only control that ends preparation reminders.

- **Mark as seen:** seen, not complete. Cancels queued text escalation and ordinary
  follow-up. Preparation reminders remain until the item is marked done. Never
  sends a reply.
- **Snooze 10 min:** postpones evaluation without resolving. The button rejects a
  snooze past the useful cutoff. An intentional override is available through
  `augmentagent imessage alerts snooze ALERT_ID --seconds 600 --override-deadline`;
  it does not extend the alert's useful lifetime.
- **Mark done / stop reminders:** ends reminders and cancels queued text escalation. A verified
  external reply also resolves a reply task in that account/thread, but cannot
  complete preparation. Merely reading an email does not resolve anything.

The clicked Discord card refreshes its heading and controls from saved state without
mentioning the owner again. Seen cards retain the done control; done cards remove
controls. Older cards remain safe to click and refresh on interaction.

Meeting cancellation resolves linked preparation. Rescheduling moves inferred
cutoffs and rearms preparation triggers, retaining the lifetime notice cap.
Explicit sender deadlines are not rewritten by calendar movement. Tracked events
missing from the normal poll window are fetched individually: absence alone is
not treated as cancellation. Expired tasks never generate stale notices.

Discord outcomes are stored in `owner_alert_notices`; state transitions are in
`owner_alert_audit`. Failed posts retry at most twice, a minute apart, using the
same enforced Discord nonce. A claim interrupted for two minutes becomes
`unknown` rather than blindly reposting; text fallback becomes eligible.
An already-started network post can finish after acknowledgment, so controls
cancel future work, not a request already at Discord. Discord nonce deduplication
is bounded by the platform's recent-message window. The persistent claim journal
prevents unbounded retry storms across process restarts.

## Phone acceptance

In Discord, enable mobile notifications for the account and this server/channel,
ensure owner mentions are not suppressed, and check OS notification permission,
Focus/Do Not Disturb, and Discord's desktop-idle mobile push delay. A bot cannot
force phone sound or visibility. Record API posting separately from phone receipt.

On 2026-10-05, a synthetic critical test posted to the configured Discord channel
at 20:19:39.420 UTC. A read-back confirmed the actual owner mention and the three
controls. The separate owner phone notification check is pending; this is not a
claim of mobile visibility. The iMessage transport acceptance and its separate
phone check are documented in PR #1394. After deployment, exercise controls through
the running gateway and record acknowledgment/state cancellation separately.

## Reported missed-email trace (redacted)

Two relevant emails arrived on 2026-10-04 at 13:27:47 and 13:32:10 UTC. They were
first seen at 13:49:45.921 and 13:50:37.200 UTC and marked processed at
13:50:37.195 and 13:51:19.657 UTC. The later reply action had one recorded nudge
and a next nudge six hours later. A corresponding next-day meeting began at
15:00 UTC, and the existing calendar alert record showed it had been detected.

A read-only Discord history check matched both action IDs to approval cards,
posted at 13:50:37.128 and 13:51:19.473 UTC, respectively. Neither card contained
an actual owner mention. This establishes ingestion, triage, and Discord channel
delivery, but not phone visibility. The existing serial approval mechanism had a
six-hour reminder interval and no preparation deadline. The evidence points to a
visibility/escalation gap; it does not establish the phone's notification settings
or exact reason it stayed unnoticed. Regression fixtures use a synthetic
colleague/project brief and test previous-day detection, reminders, classification,
owner overrides, controls, failure, restart and cancellation. Private message
content and identifiers are not included.
