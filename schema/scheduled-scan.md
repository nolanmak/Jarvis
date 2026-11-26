# Setting up a scheduled scan

A scheduled scan is a `/loop` that searches the owner's messages on a schedule for things matching criteria they describe, and delivers each new match once. Use this when the user asks you to watch for something and tell or email them about it. The user should not have to know any of the flags below: work them out from what they said.

## 1. Understand the request, and ask only what is missing

You need five things. Take every one you can from the user's message and the wiki, and ask for the rest in a single short message.

- **What counts as a match.** The one thing you usually must ask about if the request is loose. Get a concrete test ("asked about pricing", "runs an agency and mentioned a client who needs software"), not a topic.
- **Who or what to skip.** Existing customers, the owner's team, vendors, newsletters, people already handled. Propose sensible exclusions yourself and let the user correct them.
- **How often.** Default to once a day in the morning in the owner's timezone. Use the timezone in their wiki profile; ask only if none is recorded.
- **Where results go.** If they said "email me", use an address from the list at the end of this guide. If several fit and they did not say which, ask. If they did not ask for email, results post to the channel the loop is created from.
- **What each result should say.** If the results feed another person or tool, ask what layout that reader needs. Otherwise use the default layout in step 2.

Do not ask about flags, keys, windows or file paths. Those are yours to choose.

## 2. Write the criteria page in the wiki

Create `scans/<short-slug>.md` with Write. This page is the scan's whole brief: the run sees it and nothing else from this conversation. Write it as instructions to a capable reader who has never seen this conversation.

It must say:

- **Where and how far back to look.** Every indexed platform, over a window a little longer than the cadence (a daily scan looks back 2 days) so nothing falls in a gap.
- **The test for a match and the exclusions**, in the user's terms, with the reason behind the scan so borderline cases can be judged.
- **What to return.** One finding per subject, each with:
  - `key`: a stable identifier for the subject. For a person, their wiki page name, or their primary handle if they have no page. The same subject must get the same key on every run.
  - `finding`: one line. It becomes the email subject, so start it with a fixed label ("Pricing question: <name>").
  - `evidence`: the message ids relied on.
  - `body`: the text to deliver, in a fixed layout. Default layout: who they are, best contact and platform, what they said (quoted briefly), where and when, confidence (high, medium, low), suggested next step.
- **What to do when nothing matches:** return no findings.
- **That the run must not send email or messages itself.** Delivery is done by the daemon.

## 3. Preview before scheduling

Run the criteria once yourself, now, with `search_messages` over the same window, and show the user what the scan would have reported: the subjects and one full example body. Send nothing. If the results are too broad or too thin, fix the criteria page and preview again. This is the cheapest moment to get it right.

## 4. Read the plan back, then create the loop

Tell the user in plain words: what it looks for, what it skips, when it runs, where results go, and that the same subject will not be reported again for the window you chose. Create the loop only after they agree.

```
augmentagent loop create --cron "<5-field>" --tz "<IANA zone>" \
  --prompt "<short label, e.g. Pricing questions scan>" \
  --prompt-file scans/<short-slug>.md \
  --quiet-when-empty \
  --rereport-after <window> \
  --email-findings-to <address> \
  --email-account <sending account>
```

- `--prompt` is only a label shown in `loop list` and in receipts. The instructions come from `--prompt-file`.
- `--quiet-when-empty` always, unless the user wants a daily "nothing found" message.
- `--rereport-after`: how long before the same subject may be reported again. Use 90d for people (leads, contacts), 7d to 30d for things that recur (an unpaid invoice, an unanswered thread). Leave it off only when each subject should be reported once, ever.
- `--email-findings-to`: only when the user asked for email, and only an address from the list below. Any other address is refused, and you cannot add to the list. If the address they want is not on it, say so: they can connect that mailbox or add it to `AUGMENTAGENT_LOOP_SELF_SEND_ADDRESSES` on the host.
- `--email-account`: needed when more than one mail account is connected (`augmentagent gmail accounts`). Send from the account the user would expect, usually the same organisation as the destination.
- Leave out the two email flags for a scan that posts to the channel.

## 5. Report what you set up

Give the loop id, the schedule in plain words, the destination, and the path of the criteria page. Tell the user they can change what the scan looks for by asking you to edit that page; the loop does not need recreating. A cron loop first runs at its next scheduled time.

## Changing or stopping a scan

- New criteria, layout or exclusions: edit the criteria page. The next run uses it.
- New schedule, destination or window: stop the loop (`loop list`, then `loop stop <id>`) and create it again with the same `--prompt-file`.
- "Stop telling me about X": stop the loop. Leave the page in place unless asked to delete it.
