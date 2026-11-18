# Scheduled scans

A scheduled scan is a `/loop` that searches your channels on a schedule for
things that match criteria you wrote, and delivers each new match once
(#2246). It is built from four optional loop settings. A loop that uses none
of them behaves exactly as loops always have.

Example: every weekday morning, find conversations from the last day where
someone asked about pricing, and email yourself one note per person.

## Settings

| `loop create` flag | What it does |
|---|---|
| `--prompt-file <path>` | Reads the scan's instructions from a file in your wiki at every run and appends them to `--prompt`. Edit the file to change the next run. |
| `--quiet-when-empty` | A run that finishes and finds nothing posts nothing. |
| `--rereport-after <dur>` | A keyed finding is reported again once this long has passed since its last report (`7d`, `30d`). Without it, each key is reported once. |
| `--email-findings-to <address>` | Emails each new finding to one of your own addresses, with no approval card. The loop's channel gets a short receipt. |
| `--email-account <address>` | The connected account that sends those emails. Optional when one account is connected. |

## Setup

1. Write the criteria as a page in your wiki, for example
   `scans/pricing-questions.md`:

   ```markdown
   Search the last day of messages on every platform for people who asked
   about pricing. Skip anyone who is already a customer.

   Return one finding per person:
   - key: their wiki page name, or their primary handle if they have no page
   - finding: "<name> asked about pricing"
   - evidence: the message ids you relied on
   - body: who they are, what they asked, and where
   ```

2. To email findings, list your own address in `.env` and restart the daemon:

   ```bash
   AUGMENTAGENT_LOOP_SELF_SEND_ADDRESSES=you@example.com
   ```

3. Create the loop, or ask the agent to:

   ```bash
   augmentagent loop create \
     --cron "0 8 * * MON-FRI" --tz America/New_York \
     --prompt "Pricing questions scan" \
     --prompt-file scans/pricing-questions.md \
     --quiet-when-empty --rereport-after 30d \
     --email-findings-to you@example.com
   ```

`augmentagent loop list` shows a loop's settings after its prompt, and
`--json` includes them as `options`.

## Findings and keys

Every loop run replies with a small report: a status, an answer, and a list
of findings with evidence. A scan adds two optional fields to each finding:

```json
{"status": "complete", "answer": "1 new match.", "findings": [
  {"key": "example-person",
   "finding": "Example Person asked about pricing",
   "evidence": ["imessage:1234 2026-01-05"],
   "body": "Example Person asked what the monthly plan costs."}
]}
```

- **`key`** names the one subject the finding is about. Two findings with the
  same key are the same finding, whatever their wording or evidence. Keys
  compare without regard to case or spacing. A finding without a key is
  identified by its text and evidence, as before.
- **`body`** is the text to deliver. Without it the finding line is used.

Keys already reported, and not yet due again, are listed in the next run's
prompt so the run can skip them. The scheduler also enforces this itself: a
covered key is never delivered again inside its window, whatever the run
returns.

## Email delivery

The daemon sends the emails. The model never gets a way to send mail
unattended, and interactive requests still draft first and wait for your
confirmation.

- **One address, on your list.** A loop can send only to the single address it
  was created with, and only while that address is in
  `AUGMENTAGENT_LOOP_SELF_SEND_ADDRESSES`. There is no cc or bcc. The address
  is checked when the loop is created and again before every send, so
  removing it from the list stops delivery. An unset or empty list turns the
  feature off.
- **One email per new finding.** The subject is the finding line. The body is
  the finding's `body`, then its evidence, then a reference line naming the
  loop.
- **No duplicates on retry.** Each finding is recorded as reported straight
  after its email is sent. If a send fails partway, the run is retried with
  backoff from the saved answer: the model is not called again and findings
  already sent are skipped.
- **Refusals are recorded.** A send that is not allowed fails the run with the
  reason, shown as the loop's last status in `loop list`.
- **Receipt.** The loop's channel gets the count and the subjects sent, never
  the bodies.

### When the destination is a connected inbox

If the address you email is also a mailbox this daemon reads, the email
arrives there as new mail. It is recognised and skipped before any model
call: no alert, no triage and no drafted reply. Recognition needs all three
of the reference recorded at send time, the sending address, and the exact
subject, so a reply or forward that quotes the email is handled as normal
mail. The skip is recorded as the `loop_delivery` guard.

## Failure cases

| Situation | Result |
|---|---|
| Prompt file missing, empty, or outside the wiki | The run fails without a model call; the reason is the loop's last status |
| Address not on the allowlist, or no mail account | The run fails with `email delivery refused: …`; nothing is sent |
| Mail provider error | Delivery is retried with backoff; the model is not called again |
| Run returns prose instead of a report | Posted to the channel as usual; nothing is emailed |
