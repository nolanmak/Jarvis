# Migration: move the daemon to another laptop

This is a full cutover — old laptop stops running the daemon, new laptop becomes the always-on host. Memory (wiki) and history (sqlite) are preserved. Tailscale is set up as a backup path for remote access.

## The big picture

```
  OLD laptop                           NEW laptop
  ──────────                           ──────────
  1. migrate-export.sh                 
     → migrate-<ts>.tar.gz  ───────→   3. git clone + migrate-import.sh
                                       4. claude login
  2. uninstall-autostart.sh            5. install-autostart.sh
     (stop it here)                    6. install-autoupdate.sh
                                       7. Tailscale on both
```

Rule of thumb: **only one machine runs `augmentagent serve` at a time**. Two daemons polling the same Gmail account will double-triage and post duplicate approval cards.

## Step 1 — on the OLD laptop (this Mac)

### 1a. Snapshot the runtime state

```bash
cd ~/AugmentAgent
./scripts/migrate-export.sh
# → ./migrate-20260419T...Z.tar.gz
```

What's in the tarball:
- `data.db` (consistent snapshot via `VACUUM INTO` — safe to take while daemon runs)
- `wiki/` (every page Claude has written)
- `skills/email-triage/learned/*.json` (any learned skip/flag patterns)

What's NOT in the tarball, and how to move it separately:
- `.env` → transfer via a secure channel. Options:
  - **Tailscale Taildrop** (recommended once Tailscale is set up): `tailscale file cp .env <new-laptop-hostname>:`
  - AirDrop
  - 1Password/secret manager
- `~/augmentagent-vault.sparsebundle` (if you use the encrypted vault) → same transfer options; it's a directory on macOS but AirDrop handles it.

### 1b. Stop the daemon here

```bash
./scripts/uninstall-autostart.sh
```

You can leave the repo and `.env` in place for a future rollback, but the LaunchAgent should be unloaded so the daemon doesn't spring back on login.

## Step 2 — Tailscale on both machines

This is the "backup path" — lets you shell into either laptop from anywhere in your tailnet, copy files with `tailscale file cp`, and reach the Node dashboard remotely.

### On each machine:
1. Install: https://tailscale.com/download/macos (you already have it on this Mac)
2. `tailscale up` (or use the menu-bar icon → Log in) — authenticate with the same SSO account on both
3. `tailscale status` prints the tailnet IPs + hostnames. Note the new laptop's hostname.

Now the old laptop can transfer the archive:

```bash
tailscale file cp migrate-*.tar.gz <new-laptop>:
tailscale file cp .env <new-laptop>:
# On new laptop:
tailscale file get .  # pulls files into cwd
```

## Step 3 — on the NEW laptop

### 3a. Prereqs

```bash
# Xcode command-line tools (git, cc, etc.)
xcode-select --install

# Rust toolchain 1.94.1 (pinned by rust-toolchain.toml, rustup auto-installs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Claude CLI
# → download from the same place you got it on the old machine, or via your
#   preferred install method. After install:
claude login
# (a browser opens; authenticate your Max subscription)

# sqlite3 (usually preinstalled on macOS)
sqlite3 --version
```

### 3b. Clone + import

```bash
git clone https://github.com/nolanmak/MyAgentAssistant.git ~/AugmentAgent
cd ~/AugmentAgent
./scripts/migrate-import.sh ~/Downloads/migrate-*.tar.gz   # or wherever you put it
cp /path/to/secure/.env .env                                # from Tailscale/AirDrop
```

### 3c. Build + install autostart + autoupdate

```bash
cargo build --release -p augmentagent-cli
./scripts/install-autostart.sh
./scripts/install-autoupdate.sh
```

After this:
- The daemon auto-starts on login and auto-restarts on crash (every reboot, just unlock your login keychain once and it keeps running).
- Every 5 minutes the updater pulls `origin/main`. If it sees new commits it rebuilds (only if `crates/` or `Cargo.*` changed) and kicks the daemon over to the new binary. Push to GitHub from anywhere → this laptop picks it up.

### 3d. Verify

```bash
launchctl print gui/$(id -u)/com.nolanmak.augmentagent | head -30
launchctl print gui/$(id -u)/com.nolanmak.augmentagent.updater | head -20
tail -f ~/Library/Logs/augmentagent/stdout.log
```

You should see the poll loop + Discord broker connect messages, and the updater firing every 5 minutes in `~/Library/Logs/augmentagent/update.log`.

## Step 4 — smoke test the cutover

1. Send yourself a short reply-worthy email from a different address.
2. Wait up to 2 minutes.
3. Discord approval card appears in channel `1338934127079981129` (or wherever `DISCORD_CHANNEL_ID` points).
4. Click Approve → draft lands in your Gmail Sent folder.

If instead you see the card arrive TWICE: the old laptop's daemon is still running. Re-run `./scripts/uninstall-autostart.sh` on the old Mac.

## Auto-update: how it actually behaves

```
every 5 minutes:
  git fetch origin main
  if LOCAL == REMOTE: done
  else:
    git pull
    if crates/ or Cargo.* changed: cargo build --release
      if build failed: do NOT restart daemon (stays on old binary); log and exit
      if build succeeded: launchctl kickstart → daemon picks up new binary
    else: launchctl kickstart anyway (wiki/schema changes could want a refresh)
```

Log at `~/Library/Logs/augmentagent/update.log`. Each run writes a short timestamped summary. If a build breaks main, the daemon keeps running on the old code — push a fix and the next tick picks it up.

To pause auto-update without removing it: `launchctl bootout gui/$(id -u)/com.nolanmak.augmentagent.updater`. To re-enable: `./scripts/install-autoupdate.sh` (idempotent).

To disable it entirely: `./scripts/uninstall-autoupdate.sh`.

## Rolling back to the old laptop

If the new laptop gives you trouble, cut back:

```bash
# On NEW laptop:
./scripts/uninstall-autostart.sh
./scripts/uninstall-autoupdate.sh

# On OLD laptop:
./scripts/install-autostart.sh
# (data.db / wiki here may be slightly stale — the new laptop's state since
#  cutover isn't mirrored back. Either accept the drift or run migrate-export
#  on the new laptop and migrate-import here to catch up.)
```

## Tailscale as backup access

With both machines on the tailnet:

- **Shell in**: `ssh <your-user>@<new-laptop-tailnet-hostname>`. You can `tail` logs, restart the daemon, `git pull` manually.
- **Dashboard in a browser**: the Express dashboard binds `0.0.0.0:3000` by default. From anywhere on the tailnet, `http://<new-laptop-hostname>.<tailnet>.ts.net:3000` loads it.
- **macOS firewall**: if it's on, allow incoming connections for `node` in System Settings → Network → Firewall. Without this the dashboard is only reachable from localhost on the new laptop.
- **File transfer**: `tailscale file cp ./some-file <hostname>:` is easier than scp for quick snapshots.

## FAQ

**What happens if the auto-updater and I both push at the same time?** The updater just runs `git pull --ff-only`. If there's a conflict, pull fails, the log notes it, and the updater bails until you resolve the branch state manually.

**What if Rust fails to build on the new laptop?** First run: check that `rust-toolchain.toml` installed the right channel (`rustup show`). If compilation fails on a specific crate, check the `target/` cache and `cargo clean -p <crate>`.

**What if I want to run both laptops simultaneously by mistake?** Nothing stops you. Both will poll Gmail, both will create drafts, both will post approval cards. Gmail's `messageId` is unique so the sqlite PK prevents double-inserts, but the Composio side will have duplicate drafts. Avoid.

**Can the new laptop be headless (closed lid, mostly asleep)?** macOS agents stop when the lid is closed unless you set the system to never sleep on AC power (System Settings → Battery → Options → Prevent automatic sleeping when display is off = on). For a laptop deployment, plug it in + keep lid open, or invest in a cheap Mac mini.

**Will the encrypted vault (sparse bundle) work on the new laptop?** Yes, but you have to transfer the sparse bundle itself (it lives at `~/augmentagent-vault.sparsebundle` — outside the repo, not in the archive). Then `security add-generic-password` the passphrase on the new laptop. See `docs/VAULT.md` for the full setup.
