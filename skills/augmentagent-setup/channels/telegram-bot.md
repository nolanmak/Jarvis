# Telegram Bot Setup

## Category

token paste

## When to use

User wants AugmentAgent to talk over a Telegram bot (DM the owner, post
to subscribed chats, take simple commands). Status JSON reports
`channels.telegram.configured = false` and the user wants Telegram on.

## Prereqs

- A Telegram bot token from BotFather (`@BotFather` -> `/newbot` ->
  follow the prompts -> paste the resulting `<id>:<secret>` token).
- The user's numeric Telegram user id (DM `@userinfobot`, copy the
  numeric id). This becomes `AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID` and
  enables owner-DM auto-subscribe.
- Optional: `AUGMENTAGENT_TELEGRAM_BOT_AUTH` file path for headless/CI
  hosts where the D-Bus secret service is unavailable; default storage
  is the keychain.

## Steps

1. AskUserQuestion: tell the user to create a bot via BotFather and
   paste the resulting token. Mask the token (treat as secret). The
   token format is `<numeric-id>:<base64-secret>`.
2. AskUserQuestion: ask for the owner numeric user id (DM
   `@userinfobot`). The user pastes it into `.env` as
   `AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID=<id>`. The skill is read-only
   on `.env`; the user edits by hand.
3. Run the login:
   ```
   augmentagent telegram-bot login --token <token>
   ```
   The CLI calls `getMe` to validate, persists to the keychain slot
   `augmentagent/telegram-bot/<bot_username>`, and writes the row in
   `telegram_bots`. It also reads `AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID`
   from the environment; if unset, owner-DM auto-subscribe stays off
   and the CLI prints a warning.
4. Restart the daemon so the new bot row goes live:
   ```
   augmentagent service restart
   ```
   Confirm with AskUserQuestion first.

## Validate

```
augmentagent status --channel telegram --json
augmentagent telegram-bot bots --json
augmentagent telegram-bot list-chats --json
```

`status` shows `configured = true` once a bot row exists.
`bots --json` lists active bots with their owner_chat_id. `list-chats`
lists subscribed chats plus the owner DM. The live confirmation is to
DM the bot from Telegram and watch:

```
augmentagent logs --unit augmentagent.service
```

You should see the bot's getUpdates call returning the test message.

## Common errors and fixes

- "token doesn't look like a BotFather token". The token is missing
  the colon separator. BotFather tokens are always `<id>:<secret>`.
  Re-copy from BotFather, retry.
- "getMe probe failed (token invalid?)". The token is revoked or
  malformed. In BotFather, `/mybots` -> pick bot -> `API Token` ->
  Revoke -> generate a fresh one, retry.
- "keychain round-trip after save failed". The D-Bus secret service is
  not available on this host. Either unlock the keyring or set the
  file-fallback `AUGMENTAGENT_TELEGRAM_BOT_AUTH=<path>` in `.env`,
  restart, retry login.
- "AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID not set" warning. The login
  succeeds but owner-DM subscriptions are disabled. The user can DM
  `@userinfobot` to get their numeric id, paste it into `.env`, restart.

## Disarm / undo

Telegram has no arming gate. To remove a bot:

```
augmentagent telegram-bot remove --bot-username <name>
```

This best-effort deletes the keychain slot and deactivates the
subscription rows. The bot itself remains in BotFather; the user can
revoke or delete it there.
