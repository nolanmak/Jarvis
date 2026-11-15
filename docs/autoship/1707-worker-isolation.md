# Worker checkout isolation (#1707)

The worker uses the existing harness-owned controller, fetched from remote main,
for normal issue runs, dry runs and merge sweeps. Dry runs still execute tools
and gates, so they need the same isolation. Candidate paths remain shared by the
existing workflow key and lease; this change adds no second builder or allowance
accounting. The S outcome contract classifies checkout failures as configuration
faults.

## Preserved work and durable identity

A controller refresh refuses both dirty state and clean commits not contained in
remote main. It never resets, stashes or commits interactive owner work. Existing
candidate recovery retains tracked, staged and untracked edits. Preparation Git
operations disable inherited owner hooks; commits and publication keep their
existing hooks. Fetches avoid rewriting the owner's FETCH_HEAD.

Private, atomically replaced and fsynced receipts under the workflow DB's parent
record the workflow/repository/branch, initial source/base and physical checkout
identity. Enrollment requires the existing current lease. Resume and pre-dispatch
checks reject foreign repositories, symlinked paths and replaced checkout roots,
including byte-identical replacements. Receipt continuity is evidence, not
publication authority: lease, audit, exact-head and existing publication gates
still apply. Pending remote completion facts retain the existing lease/audit
checks even if checkout continuity is subsequently lost.

The existing protected publication recovery can reconstruct an absent checkout.
An in-process permit captured while the path is absent allows identity renewal
only after the original lease-fenced constructor verifies the exact retained
commit, Git registration and clean checkout. An existing replacement never gets
that permit. A crash during this handoff preserves the directory and fails
closed rather than silently adopting it.

## Updater boundary

The updater checks deployment source cleanliness before sourcing repository
helpers and again before applying an update. Dirty or unverifiable source logs
`DEPLOY CHECKOUT DIRTY` or `DEPLOY CHECKOUT UNVERIFIED` and leaves builds, services
and the deployment stamp alone. Inspection disables optional Git locks and
fsmonitor; fixtures verify the owner's index and file bytes are unchanged.
The isolated worker and the deployment checkout therefore have separate safety
boundaries.

## Evidence and limits

The adjacent JSON records original RED assertions and log hashes. The managed
launcher uses `CARGO_BUILD_JOBS=2`, disabled dev/test debuginfo and
`--test-threads=2`, retaining the assigned `CARGO_TARGET_DIR`. Because this lane
was started under nohup, the test launcher restores SIGHUP to its ordinary
default; it does not change service behavior. It retries managed admission
exit 75 for up to 45 minutes.

Reproduction commands (each invoked through `scripts/cargo-managed.sh`):

```sh
# Parent b4100b5a with regression fixtures: cargo exit 101, wrapper exit 1.
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent \
  self_improve::workspace::isolation_tests -- --test-threads=2
# Dry-run entry and SIGKILL recovery, with scripted external boundaries.
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent \
  self_improve::workspace::isolation_tests::entry_tests -- --test-threads=2
# Preparation hook leak: RED before d9d860ad, cargo exit 101/wrapper exit 1.
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent -- \
  preparation_hooks_cannot_copy_owner_files_into_candidates --test-threads=2
# Actual updater regression is also wired into ordinary CLI integration CI.
scripts/cargo-managed.sh test -p augmentagent-cli \
  --test worker_checkout_isolation_cli -- --test-threads=2
# One full relevant unit suite before push.
scripts/cargo-managed.sh test -p augmentagent-cli --bin augmentagent -- --test-threads=2
```

 Real temporary
Git repositories reproduce unmerged-controller overwrite, live/restarted checkout
replacement, dry-run helper execution from the owner directory, updater helper
execution against dirty source, and inherited post-checkout hook contamination.
A real subprocess is killed with SIGKILL after lease acquisition; fake time then
expires that lease, tests unchanged-checkout recovery and replaced-path refusal,
and proves the old token cannot reserve publication. No models are invoked.

Targeted workflow/recovery tests include existing shared ownership, remote-fact
reconciliation and protected missing-checkout reconstruction. The new
`worker_checkout_isolation_cli` integration target exercises the actual updater
with tracked, staged, untracked and dirty-helper source changes. Compatibility
fixtures cover update stamps, restart hygiene, pinned artifacts, rebuild triggers,
script sync, provisioning and native policy/runtime setup.

Final verification on source `054d36bc75957c2a40f48e8e78567490a3ca1a34`: the preparation-hook regression passed, and the single full CLI unit suite passed **2,156 tests, zero failures, 44 ignored** in 1,656.73 seconds (managed/Cargo exit 0). The earlier targeted run passed 72 tests with one ignored, plus the updater integration target.

Code verification is separate from production acceptance. This lane has not
restarted services, altered deployed configuration or run the updater. Nolan or
the orchestrator must use `augmentagent deploy snapshot`, the normal updater and
existing acceptance flow after protected merge. Do not automatically clean a
dirty deployment checkout to perform that rollout; preserve owner work and have
the owner reconcile the checkout. Record deployed build identity and an isolated
issue run before claiming live acceptance. The #1704 provider rollout remains
with Nolan/the orchestrator.
