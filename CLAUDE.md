# AugmentAgent

Dual-implementation agent daemon: a Node.js/TypeScript original and a Rust rewrite with the same functionality.

## Project structure

- **Node.js/TS daemon** (`src/`): Express dashboard (port 3000), Discord bot, email polling agent using `@openai/agents` + `@ai-sdk/groq`.
- **Rust daemon** (`crates/`): Functional rewrite — email channel, Discord approval, SQLite store, wiki. Binary at `./target/release/augmentagent`.
- `.env` holds the API keys / tokens (Cerebras/Groq, Discord, Composio, SocialAPI.ai, etc.).

## Running

- TS daemon (dev): `npm run dev`
- Rust daemon: `. $HOME/.cargo/env && ./scripts/run-rs.sh serve --dry-run false`
- Dashboard: http://localhost:3000

## Building

- TS: `npm run build`
- Rust: `. $HOME/.cargo/env && cargo build --release`

## Process management

Both services run as **systemd user units** (NOT pm2 — pm2 isn't installed on
this host). PM2 commands in `package.json` are vestigial.

- Rust daemon: `systemctl --user {start,stop,restart,status} augmentagent.service`
  - Logs: `/home/nolan-makatche/.local/state/augmentagent/{stdout,stderr}.log`
- Node dashboard: `systemctl --user {start,stop,restart,status} augmentagent-dashboard.service`
- Auto-updater (`scripts/check-for-updates.sh`) runs on a timer, pulls origin/main,
  rebuilds Rust + Node when their respective sources change, and bounces each unit
  independently. Don't manually restart services for routine pulls — let it.

## Host context (single-machine deploy)

This is a **Linux-only deployment**. There is NO macOS counterpart machine.

- Don't propose `/Volumes/augmentagent`, `scp from <mac>`, sparsebundle vault,
  or any other macOS-specific path/tool.
- `scripts/vault-mount.sh` is a no-op on this host (it exits 0 on non-Darwin).
- Keychain = Linux Secret Service (gnome-keyring), accessed via the `keyring`
  Rust crate. Per-machine, not synced.
- Discord auth fallback path (env override) is wired to
  `/home/nolan-makatche/.config/augmentagent/discord-creds.json` via
  `Environment=AUGMENTAGENT_DISCORD_CREDS=...` in the systemd unit. The Linux
  bookmarklet flow (Subscriptions → Connect Discord on the dashboard) is the
  primary Discord onboarding path; the env-var path is a fallback.

## SocialAPI.ai integration

SocialAPI.ai is an **additive official backend** for the social channels
(`crates/augmentagent-channel-socialapi/`). One API key (bearer token) fronts
many connected accounts — **one "brand" account per platform** (one Instagram,
one X, etc.). It normalises official **cross-posting** and **comment/DM
read+reply** behind a single REST surface (`https://api.social-api.ai/v1/`).

- **Additive, not a replacement.** It augments the existing browser / Voyager /
  GraphQL paths; those stay. Notably, **LinkedIn personal comment replies still
  go through the existing Voyager path** — SocialAPI.ai does not displace it.
- **Auth.** `SocialApiAuth::load` reads `SOCIALAPI_API_KEY` from the env first,
  then the keyring vault slot `augmentagent/socialapi/default`. The dashboard's
  hosted-key flow (paste key → Sync accounts) is the primary onboarding path;
  routes are `/api/socialapi/*` in `src/dashboard.ts`, view at
  `views/partials/socialapi-section.ejs`. A dedicated `augmentagent socialapi`
  CLI command, `setup oauth --provider socialapi`, and a proxied OAuth path are
  forthcoming (#245, #247) and not yet merged.
- **Approval-gated.** Inbound comments (own-post) and DMs ride the same triage →
  draft → Discord approval path as every other channel. The merged code stops
  at the approval card; the actual reply *send* (#244) and cross-post fan-out
  (#241) are separate forthcoming issues. Nothing here auto-sends.
- **Caveats.**
  - Instagram accounts must be **Business/Creator and linked to a Facebook
    Page**; personal IG accounts are unsupported by the underlying API.
  - **X applies metered pricing** underneath for some send actions.
  - **Reading** comments/DMs is **free**.
  - Flat plan in use: **Side Hustle, $29/mo**.
- **Skill.** Engagement drafting guidance: `skills/socialapi-triage/SKILL.md`.

## Commit conventions

When committing on the user's behalf:
- **Do NOT** add `Co-Authored-By: Claude …` trailers.
- **Do NOT** add `🤖 Generated with Claude Code` lines.
- Author the commit as the operator directly via per-command
  `-c user.name=… -c user.email=…` (global git config is intentionally
  unset on this box). Use the GitHub noreply address tied to the operator's
  primary account so commits attribute correctly.
- Keep messages factual: what changed + why in the subject, details in the body.

## Toolchain on this machine

- Node.js v22 (nodesource), npm 10.9.4
- Rust 1.94.1 via rustup — `source $HOME/.cargo/env` before using cargo
- Build essentials and VS Code installed
- npm deps installed; Rust release binary already built

## Known issues

- Composio connection has been flaky (Gmail integration depends on it).
