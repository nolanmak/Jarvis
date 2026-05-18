# Multi-tenant agents

Run additional, fully isolated AugmentAgent instances for **other Discord
servers**, without email and without touching the production agent.

## Model

A *tenant* is a separate `serve --no-email true` process with its **own**:

- sqlite db (`~/.local/share/augmentagent-tenant-<name>/data.db`)
- wiki dir, logs, and `tenant.env` (secrets — not git-tracked)
- systemd unit `augmentagent-tenant-<name>.service`

It shares **zero** state with `augmentagent.service` (prod). The prod agent is
never read, written, or restarted-into a different state by tenant tooling. The
email crate is not modified by the multi-tenant feature at all.

### Discord: one bot, many servers

Notifications post via the **bot token** (`DISCORD_BOT_TOKEN`). One Discord bot
can be in unlimited servers. **Reuse the prod bot token**; invite that same bot
to the other server; set the tenant's `DISCORD_CHANNEL_ID` to a channel there.
No new bots, no per-tenant Discord credentials.

Tenants are **notification-only** (they post Meetup/GitHub/Drive digests). They
do not *read* Discord DMs, so the user-token `discord-dm` path stays disabled
(like LinkedIn when no creds exist) and the per-Linux-user Discord keyring is
never involved. (Reading a server would be a future phase requiring per-tenant
creds + `AUGMENTAGENT_DISCORD_NO_KEYRING` — out of scope here.)

### Composio

Reuse the existing `COMPOSIO_API_KEY`. Composio isolates each tenant by its
per-connection entity id; the Google Drive connection lives only in that
tenant's db.

## Provision a tenant

```
cargo build --release -p augmentagent-cli       # if not already built
./scripts/install-tenant.sh code-coffee          # creates unit + skeleton env
```

Then follow the printed checklist:

1. Edit `~/.local/share/augmentagent-tenant-code-coffee/tenant.env`:
   `DISCORD_BOT_TOKEN` (= prod bot), `DISCORD_CHANNEL_ID` (other server's
   channel), `COMPOSIO_API_KEY` (= existing key).
2. GitHub repos (per-tenant db):
   ```
   DB=~/.local/share/augmentagent-tenant-code-coffee/data.db
   ./target/release/augmentagent --db "$DB" github login --token <PAT> --login <user>
   ./target/release/augmentagent --db "$DB" github subscribe owner/repo --mode priority
   ```
3. Meetup: `./target/release/augmentagent --db "$DB" meetup subscribe <group-urlname> --mode digest`
4. Google Drive: run the dashboard with `AUGMENTAGENT_DB="$DB"` and click
   **Connect Google Drive** (writes the tenant's `drive_accounts` row).
5. Start: `systemctl --user restart augmentagent-tenant-code-coffee.service`
   then `journalctl --user -u augmentagent-tenant-code-coffee -f`.

## Auto-update

`scripts/check-for-updates.sh` (the prod auto-updater) restarts the prod agent
first (unchanged behavior), then additively restarts every
`augmentagent-tenant-*.service` so tenants pick up the rebuilt binary. With
zero tenants the loop is a no-op. Tenant units are **not** auto-registered by
the updater (they need secrets a git push can't carry) — provisioning is always
a deliberate operator action via `install-tenant.sh`.

## Rollback

```
./scripts/uninstall-tenant.sh code-coffee            # stop + remove unit (keeps data)
./scripts/uninstall-tenant.sh code-coffee --purge    # also delete its data dir
```

Reverting the feature merge (`git revert`) returns the codebase to prod-only;
the `--no-email` flag defaults false so the prod unit is unaffected regardless.

## Invariants (why prod is safe)

- `serve` without `--no-email` is byte-identical to before (default false).
- New channel crates (Meetup, Drive) self-gate to *disabled* when their
  subscription/connection set is empty — prod's db has none.
- New sqlite tables are `CREATE TABLE IF NOT EXISTS`, empty and unread in prod
  (same pattern as the already-shipping dormant wave-A tables).
- Tenant units are a separate filename namespace and a separate db; the
  updater's tenant loop is additive and `|| log`-guarded.
