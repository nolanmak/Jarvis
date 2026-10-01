# macOS feature parity ledger

The [machine-readable inventory](macos-parity-inventory.json) is the starting
ledger for [full parity issue #1258](https://github.com/nolanmak/Jarvis/issues/1258)
and [epic #1259](https://github.com/nolanmak/Jarvis/issues/1259). Run
`python3 scripts/macos_parity_inventory.py` to compare it with the current
source tree. The guard is also in the Linux/macOS platform workflow. A new
discovered surface without an explicit row fails the test.

The initial ledger contains 231 source-discovered entries: 71 top-level CLI
commands, 27 channel crates, 86 literal dashboard/API routes, 20 systemd
jobs, 15 shell installers, eight sidecars and four guarded tool helpers.
Every row is **unverified**. Source presence proves only that a capability
needs parity work; it does not prove it works on Linux or macOS. The Linux
baseline descriptions are hypotheses to check against current behavior. The
`owner_issue` and `test_plan` fields make that work assignable; `evidence`
stays empty until a test or real-host artifact supports a result. To mark a
row `verified`, evidence must include a 40-character commit SHA, macOS
version, CPU architecture, exact test command, passing result and artifact.

The collector currently identifies top-level CLI commands and literal routes.
Nested CLI operations, dynamic routes, dashboard/PWA user journeys, model
provider paths, Web Push, approval state transitions, scheduled sends, loops,
Apple permission prompts, and external integration round trips need further
inventory rows or explicit coverage under the discovered parent rows. Those
gaps remain open in #1258. Do not mark the parity epic complete because this
source ledger passes its coverage test.

A row can move to `verified` only after recording the commit, macOS version,
CPU architecture, exact test command, sanitized result and any required
real-host evidence. A mock or CI-only result is insufficient for a real-host
criterion. Both Apple Silicon and Intel require their own recorded evidence
before either can be declared supported for full parity.

## Apple Silicon qualification — 2026-10-01

Executed on Nolan's Mac mini over Tailscale, macOS 26.4 (25E246), arm64,
GUI user `nolmak`, in the isolated `Jarvis-platform-completion` checkout.
This records a development build; inventory rows remain unverified until
commit-pinned evidence and each row's acceptance criteria are complete.

- `cargo build --release -p augmentagent-cli -p augmentagent-mcp-memory`: passed.
- `cargo test -p augmentagent-channel-core --lib`: 525 passed, 11 ignored.
  An earlier concurrent run failed a cleanup verification; the focused
  40-call concurrency regression and subsequent full run passed. Keep this
  under observation during the required soak; do not waive cleanup failures.
- `python3 -m unittest scripts.tests.provider_supervisor_test scripts.tests.codex_tool_bridge_test.BoundedGrepTests -q`:
  17 passed. Exercises detached descendants, cancellation, controller death,
  independent sessions, stdin/cwd forwarding, bounded regex search and parent death.
- `npm ci && npm run build`: passed using Node 24.
- In `sidecars/wa-sidecar`, `go test -race ./...` and `go vet ./...`: passed.
- Native Codex subscription login: provider self-test returned `PONG`.
- Installed daemon and dashboard LaunchAgents. Authenticated dashboard
  `/api/ask` used the real provider and wiki Read tool to return the synthetic
  `MACOS_BRIDGE_READY` marker (HTTP 200). Unauthenticated access returned 401.
- The daemon uses a fresh local database/wiki and `--dry-run true --no-email true`.
  Set `AUGMENTAGENT_AUTOSTART_NO_EMAIL=true` when installing a chat-only daemon.

The Mac provider supervisor uses a private launchd job/resource coalition per
invocation; detached and reparented descendants are cleaned before issuing the
same receipt used on Linux. It requires an active GUI login session. The tool
bridge uses kqueue for parent lifetime and accommodates the macOS shared-cache
virtual address reservation when bounding search memory. GNU coreutils and Bash
are required by the shell guards and included in the service PATH.

Still open: native command confinement/build VM, Intel qualification, full
workspace qualification, reboot/sleep/soak, and real Slack/WhatsApp account
acceptance. Slack huddle support remains unproven. These results do not certify
full platform or channel parity.

### Interactive channel follow-up

On the same Mac, `cargo test -p augmentagent-channel-whatsapp -p
augmentagent-channel-slack` passed 547 tests across 41 suites (one ignored).
The Slack serve CLI suite passed seven tests; the final WhatsApp CLI suite
passed 15, including a daemon with only WhatsApp configured, delayed sidecar
connection, owner help, and rejection of stranger and own-agent messages.
The store's two WhatsApp restart/deduplication tests passed on both hosts.

A further development acceptance probe used a simulated local WhatsApp socket
with the **real Codex provider**: the first owner message read a synthetic wiki
marker, the second recalled it without a tool call. Both replies were
`MACOS_SESSION_READY`; the store retained one healthy native Codex session.
The Go WhatsApp network connection and real account were not exercised by this
probe. Its local artifact directory on the Mac is
`/tmp/jarvis-wa-native-8a6njrhu` (disposable synthetic state).
