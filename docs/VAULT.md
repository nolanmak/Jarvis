# Encrypted Vault + Discord Query — Setup

This doc covers the optional encrypted-at-rest vault (macOS sparse bundle) and the Discord query feature. Both are opt-in. The Rust daemon and Node dashboard work fine without them; enabling them locks down `wiki/` + `data.db` and lets you ask the wiki questions from Discord.

## Quick decision

| You want… | Do this |
|---|---|
| Just encryption for the wiki/db | Run `./scripts/vault-init.sh` once, then use `./scripts/run-rs.sh` + `./scripts/run-dashboard.sh` to launch things. |
| Discord querying of the wiki | Set `DISCORD_QUERY_CHANNEL_ID` and `DISCORD_ALLOWED_USER_ID` in `.env`, enable MESSAGE CONTENT intent in Discord dev portal, pass `--wiki-dir ./wiki` to the Rust daemon. |
| Both | Do both; they compose cleanly. |

## One-time vault setup

```bash
./scripts/vault-init.sh
# prompts twice for a passphrase, stores it in your macOS keychain,
# creates ~/augmentagent-vault.sparsebundle, mounts at /Volumes/augmentagent,
# moves existing ./wiki and ./data.db inside, replaces them with symlinks.
```

After this:
- `ls -la ./wiki ./data.db` shows symlinks → `/Volumes/augmentagent/...`
- `hdiutil info` lists the mounted vault
- The keychain item `augmentagent-vault` holds the passphrase

**Key handling.** Passphrase lives in the login keychain. Unlocked on login, inherited by apps launched under your user. If the Mac cold-boots with no one logged in, the next mount attempt fails with a loud error until you unlock keychain.

**Vault lifecycle.** After a reboot, run `./scripts/vault-mount.sh` (idempotent) before the daemon starts. The `run-rs.sh` and `run-dashboard.sh` wrappers do this automatically. To detach: `./scripts/vault-umount.sh`.

## pm2 recommended config (when you cut over to Rust)

Replace the existing `ecosystem.config.js` with this when you're ready to run the Rust daemon in production:

```js
module.exports = {
  apps: [
    {
      name: "augmentagent-rs",
      script: "./scripts/run-rs.sh",
      args: "serve --dry-run false --wiki-dir ./wiki",
      watch: false,
      env: { RUST_LOG: "info" },
      max_memory_restart: "256M",
      autorestart: true,
    },
    {
      name: "augmentagent-dashboard",
      script: "./scripts/run-dashboard.sh",
      watch: false,
      env: { NODE_ENV: "production" },
      max_memory_restart: "256M",
      autorestart: true,
    },
  ],
};
```

The wrapper scripts mount the vault before exec'ing each process, so pm2 restarts after a reboot recover cleanly as long as the login keychain is unlocked.

## Discord query setup

1. In the [Discord developer portal](https://discord.com/developers/applications), select your bot app → **Bot** → enable **MESSAGE CONTENT INTENT** (privileged). Save.
2. Set env vars in `.env`:
   ```
   DISCORD_QUERY_CHANNEL_ID=<channel id where you want to ask questions>
   DISCORD_ALLOWED_USER_ID=<your Discord user ID>
   ```
3. Launch the daemon with `--wiki-dir ./wiki` (enables the query handler):
   ```bash
   ./target/release/augmentagent serve --dry-run false --wiki-dir ./wiki
   ```
4. From Discord, post a message in the query channel, or DM the bot:
   > "what's my history with Acme Corp?"

The bot replies inline with a markdown answer, citing `people/<slug>.md` and other wiki pages. Responses >1900 chars are split on paragraph boundaries into sequential messages.

### How to find your Discord user ID

In Discord: User Settings → Advanced → enable Developer Mode. Then right-click your name anywhere → Copy User ID.

### How to find a channel ID

With Developer Mode on, right-click the channel in the server → Copy Channel ID.

## CLI query (local fallback)

```bash
./target/release/augmentagent --wiki-dir ./wiki wiki ask "who have I ghosted more than 2 weeks?"
```

Same reasoner, same prompt, stdout instead of Discord. Useful for cron jobs and local testing.

## Troubleshooting

**"Vault not found at ..."** — run `./scripts/vault-init.sh` first.

**"Passphrase not in keychain under service 'augmentagent-vault'"** — login keychain is locked (cold boot, remote shell, etc.). Unlock on the console and retry, or run `security unlock-keychain ~/Library/Keychains/login.keychain-db` manually.

**Dashboard won't start after reboot** — vault isn't mounted. Run `./scripts/vault-mount.sh` or use `./scripts/run-dashboard.sh` which does it for you.

**Discord bot ignores messages** — check: (a) MESSAGE CONTENT INTENT is enabled in dev portal, (b) `DISCORD_ALLOWED_USER_ID` matches your user ID (remove the var temporarily to confirm), (c) message is in `DISCORD_QUERY_CHANNEL_ID` or a DM.

**Bot replies with "wiki query failed: ..."** — check `RUST_LOG=debug` logs; most likely `claude` CLI not logged in or wiki_root doesn't exist.
