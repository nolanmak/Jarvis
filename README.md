# Jarvis

A self-hosted personal-assistant daemon. It triages your inbound messages
across many channels, drafts replies in your voice, and provides a human
approval workflow for outbound replies — plus relationship reminders, a
personal wiki, and social/posting integrations.

> **Self-hosted & single-operator.** An open-source personal-assistant daemon
> built around one operator's environment. It handles your live accounts and
> session credentials, so review the configuration and security notes before
> running it — and never commit real secrets.

## What it does

- **Triage → draft → approve.** Inbound items (email, DMs, notifications) are
  classified, a reply is drafted using a tone profile learned from your sent
  mail, and the draft is held for you to Approve / Revise / Skip. Explicit CLI
  send commands and configured automations can also perform outbound actions.
- **Many channels.** Email (Gmail), Discord, Slack, Telegram, LinkedIn,
  WhatsApp, Twitter/X, Instagram, Reddit, GitHub, Linear, Notion, Calendly,
  Google Calendar, Google Drive, Meetup, and a voice-capture channel.
- **SocialAPI.ai backend.** An official unified REST integration for
  cross-posting and reading/replying to comments + DMs across connected
  social accounts. See [SocialAPI.ai integration](#socialapiai-integration).
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
  40 crates: `augmentagent-cli` (the `augmentagent` binary), the
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

## Release status

Jarvis is an experimental, Linux-first assistant built for a single operator.
Self-hosting the daemon does not make model inference local: configured model
providers and integrations receive the context needed for their requests.
See [security notes](docs/SECURITY.md) and the
[release checklist](docs/PUBLISH.md) before connecting live accounts.

## Running

- Rust daemon (dev): `. $HOME/.cargo/env && ./scripts/run-rs.sh serve --dry-run false`
- TS dashboard (dev): `npm run dev`
- Dashboard UI: <http://localhost:3000>

## Building

- Rust: `. $HOME/.cargo/env && cargo build --release` (binary at `./target/release/augmentagent`)
- TypeScript: `npm run build`
- Tests/lint: `cargo test --workspace` · `npm test`
- Reasoner failover changes: exercise them with the fault-injection rig and
  mint the PR-gate receipt from its transcript — see
  [docs/REASONER-FAULT-INJECTION.md](docs/REASONER-FAULT-INJECTION.md)

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

Which model answers a given call is config, not code: the "Reasoner provider
chain & model tiers" block in `.env.example` documents the provider chain
(`AUGMENTAGENT_REASONER_CHAIN`, claude-only by default) and the per-provider
Quality/Fast map (`AUGMENTAGENT_MODEL_<PROVIDER>_<TIER>`). `augmentagent
doctor` reports both; `--deep` also flags a Cerebras pin that has left the
provider's catalog.

## Contributing

Branch + PR only — never push to `main` (the auto-updater watches it).
Feature work should build cleanly (`cargo check --workspace`, `npm run build`)
and keep its tests green before the PR is opened.

## SocialAPI.ai integration

[SocialAPI.ai](https://social-api.ai) is an **official, additive** backend for
the social channels. A single API key (a bearer token) fronts many connected
social accounts — one "brand" account per platform (e.g. one Instagram, one X).
SocialAPI.ai handles the per-platform OAuth and normalises two things behind one
REST surface (`https://api.social-api.ai/v1/`):

- **Cross-posting** — publish a post to a connected account through the official
  API instead of a browser/automation path.
- **Comment + DM read+reply** — list inbox comments on your own posts and DM
  conversations, and (with approval) reply to them.

It is additive: it augments rather than replaces the existing browser /
Voyager / GraphQL paths. Notably, LinkedIn personal comment replies still go
through the existing Voyager path; SocialAPI.ai does not displace it.

Reading comments and DMs is free under SocialAPI.ai; only some send actions
are metered (X applies metered pricing underneath). The plan in use is flat
(Side Hustle, $29/mo).

Everything still flows through the daemon's triage → draft → **Discord
approval** path. Inbound comments and DMs are surfaced, triaged, and a reply is
drafted, then an approval card is posted to Discord. Approving a card sends the
reply through SocialAPI.ai (#244), and cross-post fan-out turns one draft into
per-account variants behind a single approval (#241) — both merged.

Inbound arrives two ways: each channel polls (DMs every 5 min, own-post
comments every 30 min), and `POST /webhooks/socialapi` accepts pushed events
for a near-real-time path (#249). Both share the same durable dedup ledgers, so
a pushed item and a later poll of it collapse to one draft.

The engagement rubric lives at `skills/socialapi-triage/SKILL.md`.

### Setup

- **Dashboard (hosted-key flow, primary).** On the dashboard, open the
  SocialAPI.ai settings card, paste your SocialAPI.ai API key and save it, then
  click **Sync accounts** to pull your connected handles. Each handle is upserted
  into the registry; toggle accounts active/inactive or remove them inline.
  (Routes: `/api/socialapi/key`, `/api/socialapi/sync`,
  `/api/socialapi/accounts/*`.)
- **Key resolution.** Three sources, in order: the `SOCIALAPI_API_KEY`
  environment variable, then the keyring vault slot
  `augmentagent/socialapi/default`, then the sqlite `config` table under
  `socialapi_api_key` — which is where the dashboard card above writes. All
  three are read by the daemon, `doctor`, and `status` alike (#525).
- **CLI.** `augmentagent socialapi list` / `disable` / `connect`, and
  `augmentagent setup oauth --provider socialapi` (#245), which drives the
  dashboard's proxied OAuth route (#247). `augmentagent engagement watch-post
  --platform socialapi --external-id <id> --days N` is the only way to put a
  post in front of the own-post comment poller. `augmentagent compose fan-out
  --platforms socialapi` runs the cross-post fan-out.

#### Instagram requirements

Instagram accounts connected through SocialAPI.ai must be a **Business or
Creator** account **linked to a Facebook Page** — personal Instagram accounts
are not supported by the underlying API.

## License

Original Jarvis code is licensed under [ISC](LICENSE). See
[third-party notices](THIRD_PARTY_NOTICES.md) for dependency licensing and the historical grocery-provider removal.

### Automatic personal finance (Plaid)

Set `PLAID_CLIENT_ID`, `PLAID_SECRET`, and `PLAID_ENV=production` in the local
`.env` (Trial accounts use production). Bank access tokens are stored in Linux
Secret Service; the login keyring must be available to your user service.

```bash
augmentagent finance check
augmentagent finance connect --alias Household --countries US
# Open the returned Hosted Link URL and authorize your bank, then:
augmentagent finance complete --session SESSION_ID
augmentagent --wiki-dir ./wiki finance sync
augmentagent finance status
augmentagent finance summary --start 2026-09-01 --end 2026-09-30
augmentagent finance transactions --account ACCOUNT_ID --start 2026-09-01
```

Connection URLs expire; complete the Link flow promptly and retrieve its result
within six hours. To repair an expired connection, run `finance connect --alias
Household --update-item ITEM_ID`, then `finance complete` with the new session.
This preserves the existing connection rather than spending another Trial slot.

For supported US banks, add `--statements` when connecting, or use it with
`--update-item` to grant Statements consent later. Sync then archives original
PDFs and their checksums under `wiki/finance/`, requests statement refresh once
per seven days, and picks up asynchronous results on subsequent syncs.
Requesting Statements requires that bank to support the product; Transactions
can be used alone. Refreshes may incur charges on paid Plaid plans.

Install `scripts/systemd/augmentagent-finance-sync.{service,timer}` in
`~/.config/systemd/user/`, run `systemctl --user daemon-reload`, then
`systemctl --user enable --now augmentagent-finance-sync.timer` for six-hour
imports. Units assume the checkout is `~/AugmentAgent`. The existing private
wiki mirror timer handles Git backups. Keep that mirror private: its finance
pages and optional PDFs contain financial records. Tokens and the local
transaction database are not backed up by the wiki mirror.

`finance export` regenerates KB pages from local records after an interrupted
export. Query commands require no Plaid credentials and report connection
freshness. Amounts use decimal arithmetic; totals are per currency, exclude
pending transactions and Plaid-classified transfers/loan payments, and may
still include unclassified transfers. Bank descriptions are treated as data,
not instructions. The agent is allowed only `finance status`, `transactions`,
and `summary`, not connection or sync operations.
