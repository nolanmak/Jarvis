# Discord relay webhook

You can post to Discord from a coding agent (Claude Code, Codex) through a
webhook that carries your name. Discord delivers such a message as authored by
the webhook, not by you, so Jarvis ignores it by default. Listing the
webhook's id lets Jarvis answer it as a question (#2398).

## Setup

1. Find the webhook's id: it is the number in the webhook URL,
   `https://discord.com/api/webhooks/<id>/<token>`. The webhook must post in
   the query channel (`DISCORD_QUERY_CHANNEL_ID`).
2. Set it on the daemon host and restart the daemon:

   ```bash
   DISCORD_TRUSTED_RELAY_WEBHOOK_IDS=123456789012345678
   ```

   Several ids are comma-separated. If any entry is not a Discord id the
   whole setting counts as unset.

Unset, nothing changes: no webhook message is answered.

## What a relayed message can do

A message from a listed webhook, posted in the query channel itself, gets a
normal answer, as if you had typed the question there. The reply starts with
`answering a relayed message`, and the daemon logs the query with
`via_relay=true` and the webhook id.

## What it cannot do

Anyone who holds the webhook's token can post as it. A relayed message is
therefore a question and nothing more:

- **No commands.** Text that would be a command if you typed it (anything
  starting with `/` or `!`, `model …`, `loop …`) is refused with a short
  reply. It is not run and not sent to the model.
- **No approvals.** Approval buttons, slash commands and modals check the
  Discord account that clicked them. A webhook cannot click, and the relay is
  never consulted there. An action a relayed question leads to still posts an
  approval card that only you can approve.
- **Query channel only.** A listed webhook posting in a thread, another
  channel or anywhere else is ignored.
- **No attachments.** Files on a relayed message are not read.
- **Not Jarvis's own webhooks.** A webhook owned by the Jarvis bot is never
  trusted, even if listed.

The answering turn has the same tools as one of your own questions in the
query channel (it can read your data and write to the wiki). If that is more
than you want a webhook token to reach, leave the setting unset.

## Limits

- Earlier relayed questions are not included in the conversation history of
  a later turn; Jarvis's own replies are. Make each relayed question
  self-contained.
