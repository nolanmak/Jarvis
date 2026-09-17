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
- **iMessage history.** A bundled Mac exporter and scheduler import texting
  history locally or over SSH, without a second repository. See
  [iMessage setup](docs/IMESSAGE.md).
- **WhatsApp history.** Export WhatsApp Desktop conversations into searchable
  history and incremental wiki capture, with local or SSH setup and support for
  existing private Git feeds. See [WhatsApp history setup](docs/WHATSAPP-HISTORY.md).
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

## Set up Jarvis with your coding agent

Copy the prompt below into a terminal coding agent such as Codex or Claude Code.
It guides setup from a fresh machine to a verified running assistant, asks you
which providers and integrations you want, and pauses for you to sign in when
needed. Codex can be your primary provider without installing Claude Code.
This is an agent-guided setup recipe, not an unattended installer; Linux is the
supported deployment target.

```text
Help me install and configure https://github.com/nolanmak/Jarvis from scratch.
Do the setup using the terminal, explain progress briefly, and walk me through
any steps that need my input. Use the checked-out code and CLI --help as the
source of truth; do not invent commands or report unverified success.

1. Discover and plan
   Check my OS, available disk/RAM, installed tools, and any existing Jarvis
   checkout or services. Use Linux for the daemon; if this machine is unsupported,
   help me choose a Linux host before proceeding. Ask where to install, whether
   I want Codex or Claude as primary (and an optional fallback), which control
   surface/integrations to connect first, and whether to enable startup at login.
   Reuse an existing installation safely; preserve local config and data.

2. Install and configure
   Clone the repo if needed. Read README.md, .env.example, docs/SECURITY.md,
   docs/CODEX-FALLBACK.md, and the docs for my selected integrations. Inspect
   scripts before running them; adapt paths to this machine, not the author's.
   Install missing prerequisites using supported instructions for this OS.
   Build both binaries with:
   cargo build --release -p augmentagent-cli -p augmentagent-mcp-memory
   If the dashboard or chosen OAuth flow needs it, run npm ci and npm run build
   and configure the dashboard.
   Use ./target/release/augmentagent until it is available on PATH.
   Create local .env from .env.example only if absent. Keep secrets private,
   preserve existing values, and initialize the private wiki/database using
   documented CLI behavior. Never commit accounts, credentials, or wiki data.

3. Choose and authenticate the model provider
   Set AUGMENTAGENT_REASONER_CHAIN to my choice:
   codex (Codex only), claude (Claude only), codex,claude (Codex first), or
   claude,codex (Claude first). Install/authenticate only the chosen providers.
   Use each provider CLI's current help and official installation/login flow.
   Ask me to complete browser/device login or enter credentials directly into
   the local terminal/secret store; never ask me to paste secrets into chat.
   Check authentication and Jarvis's capability/tool readiness. Codex uses
   Jarvis's guarded tool bridge; keep its guards enabled. Explain any missing
   capability or usage limit rather than silently switching my provider choice.

4. Connect my selected integrations
   Inspect augmentagent setup --help, setup oauth --help, and the relevant
   channel's help/docs. Configure required callback/dashboard/sidecar services
   before starting each connection. Let me complete consent, QR scans, or local
   credential entry, then verify the connection without printing secrets.
   Set up my chosen control surface and approval routing. Leave unselected
   integrations off. Explain which actions each connection permits.
   Offer journal sync/private Git backups and auto-ship as optional follow-ups;
   configure those only if selected. Verify any knowledge-base remote is private.

5. Verify and start
   Run augmentagent doctor, augmentagent status, and focused checks for enabled
   channels; distinguish optional unconfigured services from actual failures.
   Run a synthetic read/write/tool smoke check through the selected provider in
   an isolated test workspace. Verify a dry-run request before live operation.
   Ask before sending a test message, enabling outbound automation/auto-ship,
   or starting live polling. Keep reply approvals and existing guards enabled.
   If I selected autostart, inspect scripts/install-autostart.sh before using it:
   it starts the live daemon. Verify the service, provider availability in its
   environment, and a successful poll; an active process alone is not enough.
   Do not expose the dashboard publicly as part of basic setup.

6. Hand over
   Tell me what works, which provider is primary, what's still unconfigured or
   blocked, where private data lives, and the exact commands to start, stop,
   inspect logs, update, and recover backups. Give me one first task to try.
   Do not claim completion until the checks pass; resume after authentication
   rather than leaving me with a list of commands to finish myself.
```

## Architecture

Dual implementation with shared behavior:

- **Rust daemon (`crates/`)** — the primary runtime. A Cargo workspace of
  41 crates: `augmentagent-cli` (the `augmentagent` binary), the
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

## Quickstart

From a fresh clone to your first Discord approval card, with nothing sent.
The smallest setup that produces a card is Gmail (via Composio) plus a Discord
bot.

**Prerequisites**

- Linux with gnome-keyring (Secret Service) unlocked, and `python3`, `node`,
  `secret-tool` (libsecret-tools), and `jq` (wiki mode) on `PATH`.
- Rust via rustup (the pinned toolchain in `rust-toolchain.toml` installs
  itself), a C toolchain with `perl`/`make` (OpenSSL and SQLite build from
  source), and a few GB of RAM for the release build.
- Node 22 or 24 LTS (20–25 work), and Deno ≥ 2.7 (reply drafting runs in a
  Deno sandbox).
- The `claude` CLI installed and logged in (the default reasoner chain is
  claude-only).
- A Composio API key (Gmail), a Groq or Cerebras API key (the dashboard will
  not start without one), and a Discord server you can add a bot to.

**Fork first.** Clone your fork, not this repo. `scripts/check-for-updates.sh`
pulls, builds, and restarts whatever is on `origin/main` with no signature
check — never point the auto-updater at upstream.

1. **Clone and build.** Run everything from the repo root (`.env` and
   `data.db` resolve relative to it).
   ```bash
   git clone https://github.com/<you>/Jarvis.git ~/AugmentAgent && cd ~/AugmentAgent
   . "$HOME/.cargo/env"
   cargo build --release -p augmentagent-cli -p augmentagent-mcp-memory
   npm ci && npm run build
   ```
2. **Configure.** `cp .env.example .env`, then set `COMPOSIO_API_KEY`,
   `GROQ_API_KEY` (or `CEREBRAS_API_KEY`), and optionally `AUGMENTAGENT_API_KEY`
   (your dashboard login; if empty, the dashboard generates one and prints it
   on first start). Keep `AUGMENTAGENT_GH_DISABLE=1` (stops a failed draft from
   filing a GitHub issue through `gh`). Leave the Discord variables empty for
   now. Keep keys in `.env`: `serve` reads the Composio and Discord settings
   from the environment only.
3. **Connect Gmail.** Start the dashboard with `node dist/dashboard-server.js`,
   log in at <http://localhost:3000/login>, open
   <http://localhost:3000/settings>, click **+ Add Gmail**, and finish the
   Composio consent. The dashboard binds `127.0.0.1` and the OAuth callback is
   `localhost`, so on a remote host use `ssh -L 3000:127.0.0.1:3000 <host>`.
4. **Check it** (second terminal):
   ```bash
   ./target/release/augmentagent accounts-list       # your Gmail is listed
   ./target/release/augmentagent reasoner-selftest   # one live claude round trip
   ./target/release/augmentagent doctor              # dashboard must be running
   ```
   Warnings about calendar or systemd units not being installed are expected;
   errors are not.
5. **Dry run.** `./target/release/augmentagent poll-once` prints drafts to the
   terminal: no Discord, no Gmail drafts, no sends (dry-run is the default for
   `poll-once` and `serve`). Use a test Gmail account (or one with few
   unread messages): every unread message, in any label, up to 100, is sent to
   Claude (several calls each) and then marked processed, so it never gets a
   card.
6. **Add the Discord bot.** Create an application and bot in the Discord
   Developer Portal and invite it to your server (OAuth2 URL Generator → scope
   `bot`) with View Channel, Send Messages, Embed Links, and Read Message
   History on the approval channel. With Developer Mode on, copy IDs into `.env`:
   `DISCORD_BOT_TOKEN`, `DISCORD_CHANNEL_ID` (numeric), and
   `DISCORD_ALLOWED_USER_ID` (your user ID — if unset, every click is refused).
7. **First approval card.** From another address, send the connected inbox a
   new email that asks for a reply, leave it unread, then run
   `./target/release/augmentagent poll-once --dry-run false`. It creates a
   Gmail draft and posts the card. **The bot handles button clicks while this
   command runs, so don't click Approve & Send until it has exited**; after
   that, nothing is running to act on the card. If no card appears (or it
   hangs after a Discord error), press Ctrl-C, fix the Discord values in
   `.env`, and send another fresh email — the failed one is parked for the
   retry queue and its card appears once you run `serve --dry-run false`.
8. **Go live deliberately.** `./target/release/augmentagent serve --dry-run false`
   polls every 120s; clicking **Approve & Send** now sends mail, and only
   `DISCORD_ALLOWED_USER_ID` can click. For systemd units see
   [Process management](#process-management); run
   `scripts/install-autoupdate.sh` only when `origin` is your fork.

## Running

- Rust daemon (dev, dry-run): `. $HOME/.cargo/env && ./scripts/run-rs.sh serve`
  (needs the release build; no cards or sends, but it may retry Gmail drafts
  left errored by an earlier live run). **Live mode** — real drafts, cards,
  and sends — is the explicit `./scripts/run-rs.sh serve --dry-run false`.
- Global flags such as `--wiki-dir ./wiki` go before the subcommand:
  `./scripts/run-rs.sh --wiki-dir ./wiki serve`.
- Dashboard: `./scripts/run-dashboard.sh` (runs `node dist/dashboard-server.js`;
  run `npm run build` first), then log in at <http://localhost:3000/login>.
- `npm run dev` / `npm start` run the legacy TS polling agent (dashboard plus
  its own Discord bot, Gmail sender, and `git pull` updater) — never run it
  alongside the Rust daemon.

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

`scripts/install-autostart.sh` and `scripts/install-dashboard.sh` write these
units. The daemon unit runs **live** with the wiki on
(`--wiki-dir ./wiki serve --dry-run false`), which also needs `jq` on `PATH`,
the bot's Message Content intent, and more Claude calls per email — finish
the [Quickstart](#quickstart) first.

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
  `augmentagent setup oauth socialapi` (#245), which drives the
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
