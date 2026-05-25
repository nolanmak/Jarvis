# AugmentAgent

A self-hosted personal-assistant daemon. It triages your inbound messages
across many channels, drafts replies in your voice, and routes everything
through a human approval step before anything is sent — plus a proactive CRM
layer, a personal wiki, and a growing set of social/posting integrations.

> **Private repository.** This codebase contains deployment and integration
> details for a single operator's environment. Do not make it public.

## What it does

- **Triage → draft → approve.** Inbound items (email, DMs, notifications) are
  classified, a reply is drafted using a tone profile learned from your sent
  mail, and the draft is held for you to Approve / Revise / Skip. Nothing goes
  out without explicit approval.
- **Many channels.** Email (Gmail), Discord, Slack, Telegram, LinkedIn,
  WhatsApp, Twitter/X, Instagram, Reddit, GitHub, Linear, Notion, Calendly,
  Google Calendar, Google Drive, Meetup, and a voice-capture channel.
- **Approval surfaces.** Discord is the primary control surface; a WhatsApp
  control surface and a PWA + Web Push surface are also available.
- **Proactive CRM.** A scheduled engine surfaces stale contacts, unmet
  commitments, and upcoming events as nudges, backed by a markdown
  person-wiki with an identity index (email/phone/handles → person).
- **Self-improvement & scheduling.** A `self-improve` mode can pick up
  `agent-fixable` issues and open draft PRs; a user-facing `/loop` command
  registers cron-style recurring agent tasks.

## Architecture

Dual implementation with shared behavior:

- **Rust daemon (`crates/`)** — the primary runtime. A Cargo workspace of
  ~28 crates: `augmentagent-cli` (the `augmentagent` binary), the
  `augmentagent-channel-*` channels, `augmentagent-channel-core` (the
  `Trigger`/`ChannelRunner` contract, reasoner, prompts, RateGovernor),
  `augmentagent-store` (SQLite), `augmentagent-wiki`, `augmentagent-proactive`,
  `augmentagent-approval-discord`, `augmentagent-auth` (Linux Secret Service),
  `augmentagent-browser-client`, and the content/render helpers.
- **Node/TypeScript (`src/`)** — the Express dashboard (port 3000), a versioned
  JSON API (`src/apiV1.ts`) for split deployment, and the original polling
  agent.
- **Sidecars (`sidecars/`)** — a Playwright/Xvfb browser sidecar and a
  whatsmeow-based WhatsApp sidecar, spoken to over local Unix sockets.

Other top-level dirs: `schema/` (prompt + wiki schemas), `skills/`
(hot-reloadable triage/draft fragments), `wiki/` (the person wiki),
`systemd/` (user units), `scripts/` (build/update helpers), `docs/`
(protocol/architecture notes), `views/` (dashboard templates).

## Running

- Rust daemon (dev): `. $HOME/.cargo/env && ./scripts/run-rs.sh serve --dry-run false`
- TS dashboard (dev): `npm run dev`
- Dashboard UI: <http://localhost:3000>

## Building

- Rust: `. $HOME/.cargo/env && cargo build --release` (binary at `./target/release/augmentagent`)
- TypeScript: `npm run build`
- Tests/lint: `cargo test --workspace` · `npm test`

## Process management

Both services run as **systemd user units** (not pm2):

- Rust daemon: `systemctl --user {start,stop,restart,status} augmentagent.service`
- Node dashboard: `systemctl --user {start,stop,restart,status} augmentagent-dashboard.service`

`scripts/check-for-updates.sh` runs on a timer: it pulls `origin/main`,
rebuilds the Rust and Node sides when their sources change, and bounces each
unit independently. Routine deploys go through this auto-updater — don't
restart units by hand for ordinary pulls, and don't deploy from a feature
branch.

## Configuration

Runtime secrets and integration tokens live in environment variables
(`.env`) and the Linux Secret Service (gnome-keyring), accessed via the
`keyring` crate. This is a Linux-only deployment; there is no macOS
counterpart.

## Contributing

Branch + PR only — never push to `main` (the auto-updater watches it).
Feature work should build cleanly (`cargo check --workspace`, `npm run build`)
and keep its tests green before the PR is opened.

## Grocery channel

Order groceries from Discord. v1 ships the Giant Food Stores provider
(ported from DSado88/Grocery's PRISM API client) behind a pluggable
provider interface. The sidecar lives at `sidecars/grocery/` next to the
browser and renderer sidecars, the skill prompt at
`skills/grocery/SKILL.md`, and the knowledge graph layout at
`schema/wiki-groceries.md`.

1. Fill in `.env` — `GIANT_EMAIL`, `GIANT_PASSWORD`, `GIANT_STORE_ID`,
   and optionally `GROCERY_PROXY` (Cloudflare WARP SOCKS5 to dodge
   DataDome).
2. One-time setup:
       npm run grocery:install      # sidecar deps + Playwright chromium
       npm run grocery:build
       npm run grocery:bootstrap    # interactive OTP login
3. Run the sidecar (PM2 handles it in prod, see `ecosystem.config.js`):
       npm run grocery:sidecar
4. Trigger an order via Discord ("order groceries for this week") or:
       npm run grocery:order

The agent reads `wiki/groceries/{staples,preferences,pantry,dislikes}.md`,
searches the store catalog, and posts a cart-for-review card to Discord
with Approve / Feedback / Skip buttons. It stops at the cart — the user
finishes checkout in the Giant web app — and folds feedback back into
the KG (e.g. "skip salmon next time" → `dislikes.md`).
