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
wrapper (#1754). `disk build` refuses a `--target-dir` inside its child command,
because Cargo would write to that directory instead of the leased one. In Claude
Code, the project hook `scripts/cargo-target-guard.py` blocks bare `cargo` with
`CARGO_TARGET_DIR=`/`CARGO_BUILD_TARGET_DIR=` or `--target-dir`; prefix
`AUGMENTAGENT_ALLOW_UNMANAGED_TARGET=1` only to bootstrap the storage commands.
Whatever slips past (other clients, `bash -c`, exported variables) is listed by
`augmentagent disk status`: Cargo caches directly inside a volume that holds a
managed build parent, owned by no registration, marker, or housekeeping root.
`disk status --unmanaged [--json]` prints only that list, with size, idle days,
and an `augmentagent disk register --root DIR --abandoned` suggestion; `disk prune`
prints the count. Nothing adopts or deletes them automatically. The wrapper contains test temporary files under the target's `tmp/` on the
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

Required Autoship native-VM builders acquire the global build/prune lease for
each actual VM build command, not for the whole model conversation. Reading and
thinking leave the lease available to interactive builds. VM commands retain
their private scratch-image ownership and aggregate growth reservations; they
check the configured admission/free-space floor before and during execution.
The bridge, descendant supervisor, and QEMU share the locked descriptor, so
canceling a model cannot unlock storage before its build descendants exit.
Repository `cargo-managed.sh` tool requests map to managed Cargo inside the VM;
they never execute a candidate-controlled host wrapper. Deterministic host gates
and explicit host/legacy builders retain verified target leases and parent-proof
checks. No model output can opt into the narrower admission path.

Isolated shared CI also records a private cleanup hold before its supervisor
starts. The hold binds the exact source, controller and target directories to
the resource identity observed at admission, before the command is released.
An immutable admission receipt is separate from the mutable completion result.
A damaged completion keeps CI unverified, while intact original admission and
a matching live kernel proof can still establish safe storage cleanup.
A completed process or cleanup log alone does not release that ownership:
normal completion verifies the same kernel scope is quiescent first. A
resource refusal before admission can release unused storage only when the
supervisor has reaped its descendants and never received the launch token.

If teardown cannot be proven, CI preserves its source checkout, controller and
supervisor receipts. The hold survives controller descriptor loss. Every new
managed storage admission reconciles abandoned holds under the existing
allocation lock, using the original resource identity and kernel evidence.
Missing, corrupt, replaced or foreign ownership/admission evidence defers builds and pruning; it
does not trigger code repair, spend a model call, or change shipping allowance.
Do not delete hold files, inherited locks or retained checkouts to make an
admission succeed. Recovery must establish the original correlation and
positive teardown evidence. The normal updater and worker must use the
compatible storage implementation before these holds are written.

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
idle gap between lanes. Normal managed builds serialize per state directory.
Linux deployment builds use `disk build --update --target-dir CHECKOUT/target`
to protect release outputs and dedicated scratch while auto-ship continues on
its separate gate outputs. Linked debug profiles do not block a release build.
A private per-user, per-filesystem reservation ledger atomically accounts for
each active writer's growth budget, including snapshot creation, across state
directories. Conflicting physical output leases still prevent pruning or two
writers using the same output. Legacy writers without reservations defer the
first deployment until their boundary. `AUGMENTAGENT_GATE_CACHE_MAX_MB` retains its existing
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

When age-based cleanup leaves a filesystem below its admission threshold,
managed admission and discovering `disk prune` runs reclaim rebuildable Cargo
cache directories from idle owned targets on that filesystem. This includes
recent cache data: `incremental`, `build`, `deps`, and `.fingerprint`, in that
order. The pass measures free space after each removal and stops once the
reserve plus build growth and other writers' reservations fit. Scheduled
housekeeping uses the same policy before the next build needs the space.

Pressure cleanup preserves the profile directory and everything directly in
it, including deployed and rollback binaries. It does not traverse linked
cache directories or linked external profiles, touch source/state/snapshot
directories, or remove a cache used by a build or a detected process/service
reference. Missing process/reference evidence and held physical leases keep
the cache. Reports record pressure removals and any remaining deficit; dry-run
reports estimates without deleting. If protected or unrelated data consumes
the disk, admission still refuses rather than lowering the reserve. Reclaimed
dependencies can make the next build slower, but do not require relocating or
replacing the deployed executable.

For a specific build admission, pressure cleanup considers only filesystems
containing that writer's monitored target, profile, and scratch paths. A release
build blocked on the root filesystem does not pressure-delete warm Cargo caches
on an unrelated build volume, even if that volume is also below the same
threshold. Multiple monitored filesystems remain independently eligible, and
unknown or empty filesystem scope refuses cleanup. Discovering `disk prune`
continues proactive pressure cleanup across all owned build filesystems. A
filesystem with no eligible cache still refuses admission; this does not change
the free-space floor, growth reservation, ownership, or live-build exclusions.

The same pass retires registered Autoship worktrees under
`.self-improve-worktrees` after 24 hours without writes. It holds the fresh,
resume, and per-PR run locks, requires HEAD to be reachable from `origin/main`,
and requires a clean tracked, untracked, and ignored-file inventory. Git locks,
process references (including working directories), and unavailable evidence
keep the checkout. Compiler inventory and readable process status/command lines
are mandatory. Permission-denied ptrace handles (`exe`, `cwd`, `fd`) from
non-dumpable login/key agents are optional; otherwise normal SSH sessions
would freeze all cleanup. Visible references still protect resources, and
rollback snapshot retention applies the same reference policy (#1753). Removal uses `git worktree remove` without force and retains
branches. External or unrecognized worktrees, unmerged commits (including
squash-merged heads without ancestry proof), dirty work, and ignored private
files are preserved; a deleted remote branch alone is not proof of safety.

## Updates during auto-ship (Linux)

The updater provisions `augmentagent-autopr.service` and a daemon drop-in without
changing its ExecStart. The independently managed worker uses the daemon's
environment files and dry-run setting; `AUGMENTAGENT_AUTOPR` must still be enabled
to run auto-ship. A shared `augmentagent-runtime.slice` keeps the existing total
CPU/memory/task budget across both processes. The worker inherits the daemon's
run-resource delegation so native builds keep their per-run limits.
Provisioning enables the worker but
does not start it alongside a legacy embedded loop: first startup is ordered
after the daemon restart. The initial legacy run may still defer this migration.

After migration, a daemon restart leaves the worker and its child processes
alive. The updater bypasses in-flight restart deferral only after verifying the
actual daemon's external-worker mode, process birth identity, separate service
cgroup, and ownership of every held fresh/resume run lock. Unknown or legacy
owners retain the existing restart deferral. The worker detects an executable
replacement but finishes its active run and persists accounting before reloading
between runs. A supervisor lock prevents two new workers from polling at once.

`disk build` exits 75 for contention, 73 for capacity/budget refusal, and 76 when
preflight cannot establish safety. The updater logs these as distinct deferrals,
withholds its deployment stamp, and retries on its next normal tick. Compiler
errors still fail the build. Compatibility handling recognizes the old binary's
mislabelled lock timeout without treating compiler diagnostics as contention.
`disk-reconcile.json` also reports headroom reserved by other writers. macOS
retains the embedded loop and serialized storage path until launchd handoff is
implemented and verified; see issue #1599 and the parity inventory.

## Snapshots and rollout backups

Use `augmentagent deploy snapshot --label rollout-NAME` for future rollouts.
The helper creates a consistent compressed SQLite copy, optional release binary,
and ownership manifest. It runs retention around creation. Snapshot allocation
shares the build lease and reserves space for simultaneous plain/compressed
copies, the binary, and the final free-space floor. The storage budget
`AUGMENTAGENT_DEPLOY_MAX_GB` (default 8 GiB) is checked against the new
snapshot's exact size once it is written, not an estimate (#1815).

Retention keeps the newest `AUGMENTAGENT_DEPLOY_KEEP` (default two) snapshots
*per source database*; each manifest records `source_db`. Manifests written
before that form one legacy group, which stops holding keep slots once that
many tagged snapshots are newer than all of it; a group whose database file is
gone stops too. While under budget, snapshots younger than 48 hours are also
retained. Over budget, the budget wins: unpinned, unreferenced snapshots
outside the keep slots are evicted oldest first until the total fits, and
nothing is evicted if even that would not fit. Creation evicts only after the
new copy is complete, so a failed copy never costs a rollback point. Forensic
evidence counts toward the budget and is never evicted for it. When protected
material alone fills the budget, `deploy snapshot` refuses, names each blocker
(`pinned`, `referenced`, `reference probe unavailable`, `forensic evidence`,
`newest for its database`, `newest legacy snapshot`), and tells agents to
report it rather than unpin or raise the cap; `autopr-health` raises
`snapshot-budget` for the owner. Retention orders snapshots by the manifest
creation date; pinning or unpinning does not change which rollback points
count as newest.

```sh
augmentagent deploy pin SNAPSHOT --reason 'current rollback point'
augmentagent deploy unpin SNAPSHOT
augmentagent deploy prune --dry-run --json
```

Pins, running executables, open files, command arguments, and service references
prevent expiry. Unreadable reference evidence keeps candidates; the one
exception is the permission wall the kernel puts around non-dumpable login/key
agents (`sshd`, `ssh-agent`, `(sd-pam)`), whose `exe`/`cwd`/`fd` are denied even
to their own user while their status and command line stay readable (#1753).
Each kept snapshot says why: `kept: pinned`, `kept: referenced`, or
`kept: reference probe unavailable (<first blocker>; +N more)`. A pin is
reported ahead of the others. Over budget with nothing evictable, prune prints one `kept:` line per blocker and `over budget: … nothing evictable`. Unknown folders
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
