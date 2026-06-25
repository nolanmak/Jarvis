# Encrypted Vault + Discord Query — Setup

This doc covers the optional encrypted-at-rest vault (macOS sparse bundle) and the Discord query feature. Both are opt-in. The Rust daemon and Node dashboard work fine without them; enabling them locks down `wiki/` + `data.db` and lets you ask the wiki questions from Discord.

> **Scope:** The encrypted vault is a **macOS-only, opt-in** feature. The standard production deployment is **Linux + systemd user units** (see README → *Process management*). On Linux the daemon runs against plaintext `./wiki` and `./data.db`, and `vault-mount.sh` is a no-op.

## Quick decision

| You want… | Do this |
|---|---|
| Just encryption for the wiki/db | Run `./scripts/vault-init.sh` once, then use `./scripts/run-rs.sh` + `./scripts/run-dashboard.sh` to launch things. |
| Discord querying of the wiki | Pass `--wiki-dir ./wiki` to the Rust daemon and enable MESSAGE CONTENT intent in the Discord dev portal. Optionally set `DISCORD_QUERY_CHANNEL_ID` / `DISCORD_ALLOWED_USER_ID` in `.env` to scope it. |
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

**Path/service overrides.** The values above are defaults, not hardcoded. The scripts read these env vars:

| Env var | Default |
|---|---|
| `AUGMENTAGENT_VAULT_PATH` | `~/augmentagent-vault.sparsebundle` |
| `AUGMENTAGENT_MOUNT_POINT` | `/Volumes/augmentagent` |
| `AUGMENTAGENT_VOLNAME` | `augmentagent` |
| `AUGMENTAGENT_VAULT_SERVICE` | `augmentagent-vault` |
| `AUGMENTAGENT_VAULT_SIZE` | `2g` (sparse, grows on demand) |

## Autostart (production)

Register both services with the OS process manager — a launchd LaunchAgent on macOS, a systemd user unit on Linux. The installers are idempotent (re-running reloads with the current config):

```bash
./scripts/install-autostart.sh    # Rust daemon (launchd: com.nolanmak.augmentagent / systemd: augmentagent.service)
./scripts/install-dashboard.sh    # Node dashboard
```

Both installers launch through the `run-rs.sh` / `run-dashboard.sh` wrappers, which call `vault-mount.sh` before exec — so the vault is already mounted before each process starts. Restarts are handled by launchd `KeepAlive` (macOS) / systemd `Restart=on-failure` (Linux), so after a reboot they recover cleanly as long as the login keychain is unlocked (macOS) / Secret Service is available (Linux). To remove the services: `./scripts/uninstall-autostart.sh`.

> The legacy `ecosystem.config.js` (pm2) runs the old Node `augmentagent` + fetch-sidecar, not the Rust daemon — it is superseded by the autostart installers above and not the recommended path for the Rust daemon.

## Discord query setup

1. In the [Discord developer portal](https://discord.com/developers/applications), select your bot app → **Bot** → enable **MESSAGE CONTENT INTENT** (privileged). Save.
2. (Optional) Set env vars in `.env` to scope the feature:
   ```
   DISCORD_QUERY_CHANNEL_ID=<channel id where you want to ask questions>  # optional; defaults to DISCORD_CHANNEL_ID
   DISCORD_ALLOWED_USER_ID=<your Discord user ID>                         # optional; unset means no user filter
   ```
3. Launch the daemon with `--wiki-dir ./wiki` — this is what actually enables the query handler:
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
