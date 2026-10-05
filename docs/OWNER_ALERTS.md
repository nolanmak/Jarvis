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

Priorities are exact bare email addresses: `high`, `critical`, or `routine` (mute
new proactive classification). Changing a priority reconsiders prior eligible
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

## Controls and recovery

- **Acknowledge:** seen, not complete. Cancels queued text escalation and ordinary
  follow-up. Preparation reminders remain until resolution. Never sends a reply.
- **Snooze 10 min:** postpones evaluation without resolving. The button rejects a
  snooze past the useful cutoff. An intentional override is available through
  `augmentagent imessage alerts snooze ALERT_ID --seconds 600 --override-deadline`;
  it does not extend the alert's useful lifetime.
- **Resolved:** ends reminders and cancels queued text escalation. A verified
  external reply also resolves a reply task in that account/thread, but cannot
  complete preparation. Merely reading an email does not resolve anything.

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
