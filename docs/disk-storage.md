# Build and rollback storage

Every agent build must participate in the shared storage lease. The gate and
auto-PR builder do this automatically. For interactive sessions, use:

```sh
. "$HOME/.cargo/env"
./scripts/cargo-managed.sh test -p augmentagent-cli
```

`disk build --target-dir PATH -- COMMAND ...` is the underlying command. It
registers a target in the private state directory, validates its cache tag,
reconciles storage, then holds the build/prune lease until COMMAND exits.
Without overrides, the wrapper enrolls a dedicated session-cache parent and
chooses a stable child per worktree. On this Linux host it prefers the writable
`/mnt/build` volume, falling back to the user cache directory when unavailable.
Set `AUGMENTAGENT_BUILD_PARENT` to place that
parent on a build volume, `CARGO_TARGET_DIR` for a specific cache, or
`AUGMENTAGENT_STORAGE_BIN` for a supporting binary during bootstrap.
Do not start an external `CARGO_TARGET_DIR=... cargo ...` build outside this
wrapper. The wrapper contains test temporary files under the target's `tmp/` on the
measured filesystem and reclaims scratch before its next admission. It defaults
dev/test debuginfo to zero and strips provider secrets
from build subprocesses using the gate policy. It also finds the deployed
binary from the shared Git directory when invoked in a worktree.

On Linux, nested wrapper calls inside a managed command or auto-PR builder
reuse the ancestor's lease for the same target. The CLI verifies the live
ancestor's held state and physical target locks; an environment flag alone
cannot bypass admission. Nested commands preserve live scratch and stop if
that protection disappears. A different target requires a separate admission
after the enclosing build finishes. On other platforms, use Cargo directly
with the inherited target inside an already managed command; native evidence
for nested lease reuse is unavailable.

For ad hoc session names, opt a dedicated build parent into discovery once:

```sh
augmentagent disk manage-parent --root /path/to/jarvis-session-targets
```

Immediate child Cargo caches are then discovered automatically and expire after
three quiet days. This is an explicit ownership declaration for that parent;
use a directory dedicated to this project. Unmanaged siblings elsewhere and
source directories are preserved. The persistent gate registration overrides
this ephemeral policy if it is located within the parent.

An existing target can be explicitly adopted:

```sh
augmentagent disk register --root /path/to/cache --worktree /path/to/owning-worktree
augmentagent disk register --root /path/to/shared-cache --persistent
augmentagent disk register --root /path/to/verified-abandoned-cache --abandoned
```

A Cargo cache tag alone does not grant cleanup permission. External targets need
both a durable registration and matching target marker. Explicit registration can
repair a missing tag only when a compiler record and real Cargo profile layout
verify that it is a legacy cache; unknown nonempty folders are rejected.
The configured gate remains explicitly owned independently of registration and
can be shared across repositories without changing its owner. Removed worktrees
remain discoverable through the registration. Their targets, and explicitly adopted abandoned caches with unknown worktree
names, expire after three quiet days; live worktrees use the ordinary artifact retention. Symlink
replacement, missing evidence, recent writes, and running builds prevent whole
root deletion. Registration records contain paths, not source or credentials.

Admission reconciles before each build, eliminating dependence on finding an
idle gap between lanes. Managed builds are serialized to avoid spending the same
free-space reserve twice. `AUGMENTAGENT_GATE_CACHE_MAX_MB` retains its existing
20000 default. `AUGMENTAGENT_GATE_MIN_FREE_GB` defaults to 15 GiB;
`AUGMENTAGENT_BUILD_GROWTH_GB` adds 10 GiB of anticipated in-flight growth before
admission. Size the latter from measured peak growth for larger builds.
Managed commands,
gates, and builder calls monitor available space while running and stop at the
final floor, before consuming the remaining reserve.
A refused admission remains an infrastructure failure. `disk-reconcile.json`
records the last admission's measured before/after space and cleanup results.
`disk-prune-last.json` records periodic cleanup outcomes and fresh filesystem readings.
The daily pruner and pressure-triggered health reconciliation use the same
exclusion and report deferral when a build owns the lease. The updater uses
`disk build` for release builds once the deployed binary
supports it, and never falls back to deleting a cache outside the pruner's lease.

## Snapshots and rollout backups

Use `augmentagent deploy snapshot --label rollout-NAME` for future rollouts.
The helper creates a consistent compressed SQLite copy, optional release binary,
and ownership manifest. It runs retention around creation. Snapshot allocation
shares the build lease and reserves space for simultaneous plain/compressed
copies, the binary, and the final free-space floor. The storage budget
`AUGMENTAGENT_DEPLOY_MAX_GB` defaults to 8 GiB; admission conservatively reserves
the uncompressed input plus binary. The newest `AUGMENTAGENT_DEPLOY_KEEP`
snapshots (default two) and snapshots younger than 48 hours are retained. If
protected data exhausts the budget, creation refuses rather than growing it.
Retention orders snapshots by the manifest creation date; pinning or unpinning
does not change which rollback points count as newest.

```sh
augmentagent deploy pin SNAPSHOT --reason 'current rollback point'
augmentagent deploy unpin SNAPSHOT
augmentagent deploy prune --dry-run --json
```

Pins, running executables, open files, command arguments, and service references
prevent expiry. Unreadable reference evidence keeps candidates. Unknown folders
under the snapshot root are preserved. Stray backup deletion still requires
`--strays --yes`.

Historical rollout directories are never adopted by name alone. To explicitly
register a direct child of the private state directory for this retention:

```sh
augmentagent deploy adopt-rollout --path /private/state/augmentagent/rollout-NAME
```

Adoption starts pinned. Check its rollback and credential dependencies before
unpinning. This command neither copies nor deletes the original. Adopted folders
use their most recent pre-adoption write as their retention age, with the
enrollment date recorded separately. Future rollouts
should use compressed `deploy snapshot` instead. `deploy list` includes owned
external data in its storage accounting and identifies pins.

## Rollout verification

Deploy through the normal updater. Do not pin a service to a temporary binary.
Wait for legacy builds to finish before switching to the coordinated entry
points; they do not hold the new lease. Verify gate/builder and session paths,
then inspect `disk prune --dry-run --json`, `disk status --json`, the last
reconciliation report, the installed timer, and multiple successive build cycles.
An issue remains open until this host verification is recorded. A passing fixture
suite alone does not prove that the running service uses the new implementation.
