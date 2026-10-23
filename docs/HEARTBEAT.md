# Heartbeat

The heartbeat is a periodic, open-ended check-in (#1317). On a cadence, the
agent reads your checklist at `<wiki>/HEARTBEAT.md` together with a small
snapshot of recent activity. It then either stays silent, which is the normal
case, or posts one notice card to the approval channel.

Everything else Jarvis schedules is task-specific: proactive scans, channel
polls, the digest and `/loop`. The heartbeat is the one that asks "given all
of this, does anything need me right now?" It can also decide to say nothing.

It is modeled on [OpenClaw's heartbeat](https://github.com/openclaw/openclaw)
and [Hermes Agent's](https://github.com/NousResearch/hermes-agent) `/heartbeat`
and cron silence contract, and it takes on the fixes for their known failure
modes. Issue #1317 has the full comparison.

## Setup

1. Write a checklist at `<wiki>/HEARTBEAT.md`. Short, concrete lines work
   best:

   ```markdown
   # Heartbeat
   - Tell me if a flight or hotel booking changed.
   - Flag an investor or customer email that has waited more than a day.
   - If a calendar event in the next 2 hours has no location or link, say so.
   ```

   A file that is missing, or holds only headings, comments, blank lines or
   empty list items, is skipped **without a model call**. You can leave the
   feature enabled and pause it by emptying the file.

2. Try it without delivering anything:

   ```bash
   augmentagent --wiki-dir ./wiki heartbeat run-once --dry-run --force
   ```

3. Enable it in `.env` and restart the daemon:

   ```bash
   AUGMENTAGENT_HEARTBEAT_ENABLED=1
   AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS=08:00-22:00
   AUGMENTAGENT_HEARTBEAT_TZ=America/New_York
   ```

The daemon (`serve`) starts the loop only when it is enabled, `--wiki-dir` is
set and it is not a dry run. At startup it logs `heartbeat disabled: <why>`
otherwise.

## Configuration

| Variable | Default | Notes |
|---|---|---|
| `AUGMENTAGENT_HEARTBEAT_ENABLED` | off | `1`, `true`, `yes` or `on` |
| `AUGMENTAGENT_HEARTBEAT_INTERVAL` | `30m` | `45s`/`30m`/`2h`/`1d` syntax, with a 5m floor |
| `AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS` | all day | `HH:MM-HH:MM`, start inclusive, end exclusive, `24:00` allowed; `22:00-06:00` crosses midnight; equal start and end means never |
| `AUGMENTAGENT_HEARTBEAT_TZ` | host local | IANA zone for the window and for the time shown to the model |
| `AUGMENTAGENT_HEARTBEAT_DAILY_CAP` | `6` | Maximum notices delivered per rolling 24h |
| `AUGMENTAGENT_HEARTBEAT_TIMEOUT_SECS` | `300` | Wall-clock budget for one model call |

A value that doesn't parse falls back to its default and produces a warning.
The warning is logged at startup and shown by `heartbeat status`. An invalid
window means "all day", not "never".

## What one run does

The loop checks every minute whether a run is due. The cadence is measured
from the last attempt stored in the database. As a result, restarts neither
double-fire nor reset the clock, and a long outage produces one catch-up run
rather than a burst.

Each due run goes through these gates in order. Everything before step 6
costs nothing:

1. **Lease.** If another process holds the run (for example the daemon while
   you use `run-once`), the run is skipped with `busy`.
2. **Active hours.** Outside the window, the run is skipped with `quiet-hours`.
3. **Checklist.** Missing or effectively empty skips with `empty-checklist`.
4. **Cap.** Once the daily notice cap is reached, runs are skipped with `cap`.
5. **Record.** A `running` row is written *before* the model call, so a crash
   can't cause a double-fire. A row left over from a dead process becomes
   `error/interrupted` on the next run.
6. **Model call.** Each run is a fresh session on the fast tier, with
   read-only wiki tools (`Read`, `Grep`, `Glob`), under the configured timeout.
   The prompt contains:
   - the checklist;
   - the local time;
   - the time since the last run;
   - the last notice sent;
   - the number of waiting approval cards;
   - up to 25 inbound items since the last run.
7. **Decision.** See the next section.
8. **Duplicate check.** If the same notice was delivered in the last 24h, the
   run is skipped with `duplicate`. The comparison ignores case and spacing.
9. **Delivery.** The notice is sent as one card, truncated to 500 characters.
   It is recorded as `sent` only after the card posts. If posting fails, the
   run is `error/delivery`, and the next run is not treated as a duplicate.

After three errors in a row, the heartbeat posts **one** "Heartbeat failing"
card. It posts nothing more until a run succeeds. Runs are kept for 30 days.

## Decision contract

The model must reply with exactly one JSON object:

```json
{"notify": false}
{"notify": true, "message": "Flight UA12 moved to 6pm; your 5pm pickup needs changing."}
```

- JSON inside a code fence, or after other prose, is accepted. If there are
  several objects, the last one wins.
- `HEARTBEAT_OK` at the start or end of the reply is accepted as a synonym
  for silence (OpenClaw compatibility). It is rejected when more than 300
  characters of other text come with it.
- Anything else, including free prose, is recorded as `error/invalid-output`
  and is **never delivered**. OpenClaw's token-only contract let chatty models
  leak narration into notices (their #142588); failing closed avoids that.

## CLI

```bash
augmentagent --wiki-dir ./wiki heartbeat run-once [--force] [--dry-run] [--json]
augmentagent --wiki-dir ./wiki heartbeat status [--json] [--check]
```

- `run-once` goes through the same gates as the loop.
  - `--force` ignores the interval and active hours. The checklist, cap,
    duplicate check and lease still apply.
  - `--dry-run` calls the model but delivers nothing and records nothing.
- `status` shows the configuration, whether the checklist is present, any
  warnings, and the last 10 runs.
- `status --check` is a liveness probe. It exits `1` when the heartbeat is
  enabled but has not attempted a run for three intervals. The rule applies
  only once the active window has been open for at least that long, so it
  doesn't alarm the moment the window opens. For example, from cron:

  ```bash
  */30 * * * * cd ~/AugmentAgent && ./target/release/augmentagent --wiki-dir ./wiki heartbeat status --check >/dev/null || curl -fsS https://hc-ping.com/<uuid>/fail
  ```

## Cost

At the default of 30 minutes over a 14-hour active window, the heartbeat makes
at most about 28 fast-tier calls a day. The empty-checklist, quiet-hours and
cap gates run before the call, and each run starts a fresh session instead of
carrying history. Those two choices kept OpenClaw's idle cost down (#81186,
#98556).

Not built yet: an idle gate that backs off when nothing has changed, and
waking the heartbeat on an incoming high-priority event. Both are follow-ups
on #1317.
