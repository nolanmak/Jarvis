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
stays empty until a test or real-host artifact supports a result.

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
