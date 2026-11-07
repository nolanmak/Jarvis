# Resource closure required before retained-session credit

This is an investigation amendment to
`1902-retained-native-write-fence-design.json`, not an implementation handoff or
permission to install a privileged helper. It specifies the boundary that any
controller/journal design must satisfy before it can reduce a reservation.

## The counterexample changes the controller premise

The disposable guest now contains a real open-but-unlinked regular file. The
experiment captures only `owned.json` and `lock`, acquires actual read leases on
both, applies immutable to both and their directory, and validates the original
named and held identities, inventory and contents. An actual subprocess then
writes and fsyncs 4 KiB through the unlinked inode's inherited writable FD.
Both logical size and allocated blocks grow while the visible inventory, all
visible immutable flags and both read leases remain unchanged.

See `1902-unlinked-writer-counterexample.json` for the exact source, log and
original supervisor evidence. This does not establish that a production session
has such an FD. It establishes that a visible inventory plus flags/leases does
not prove that all resources charged to that session are incapable of growth.
Passing the test means the counterexample was reproduced; it is not a passing
capacity-recovery test.

The six immutable syscall observations, ten capture interleavings and four
flushed restart boundaries remain scoped observations. A privileged setter
cannot turn those observations into complete resource closure merely by
recording them in a root-owned journal.

## Required producer/consumer join

R must produce an actual private, borrowed resource-bound capability before a
custody accounting consumer can lower an old session's demand. This capability
does not yet exist. Its constructor must inspect and correlate the original
resource authority, not accept a caller's JSON, file list, PID list, heartbeat,
source label or `complete_fence` boolean. It must bind:

1. The original native store identity, runtime identity and original resource
   ownership/launch correlation. Resource closure is separate from original
   input nonrelease, execution outcome, replay, review and merge authority.
2. The complete charged resource population: visible and unlinked inodes,
   writable mappings, duplicated/inherited descriptions, external scratch or
   copies, helpers and descendants, and shared mutable controller metadata.
   Every excluded cost must instead retain a separate conservative reservation;
   an unmeasured or uncorrelated cost cannot silently become zero.
3. An actual mechanism excluding or bounding every remaining future growth
   route, including delayed starts after temporary locks/leases close. A
   momentary process/FD census is not that mechanism. PID absence and a source
   path substring cannot manufacture the original ownership correlation.
4. The exact immutable inode/inventory transition where it applies, its current
   kernel/filesystem support, persistence and live revalidation. The resource
   proof and native lock must remain live across the final accounting/admission
   decision. Later invalidation follows D13.

Two possible research directions remain unapproved: original R evidence that
positively closes the relevant original resource lifetime; or a real kernel
resource bound covering every remaining charged resource, including hidden
ones. Either needs its own original authority producer, fault tests and full
integration audit. A new scan or a privileged file setter alone is neither.
No new privileged census, process freeze, resource migration, quota change or
production filesystem transition is authorized by this amendment.

Until this producer exists, the future-growth demand stays at its full ordinary
value even if every visible immutable flag is set and its journal is intact.
The original outcomes, data and aliases remain unchanged.

## Original executable reproduction and remaining runtime join

`1902-original-producer-reproduction.json` records a whole-file reproduction of
the preserved original executable. An unchanged checkout of
`fe2e7b1f50430af73e65bb38049097e959b7a3e3`, built through actual managed storage
leases with the original logical paths and observed phone telemetry build input,
produced SHA256
`7bbdd56db0c4b6f926d67dad7620104b00cc373ed8cb5b51a29d051be9baad30`.
Both complete hashes and `cmp` agree. No code, data, build ID or other section
was excluded. The telemetry value is an existing supported build input, not a
claim that its named Git revision supplied the compiled source.

This resolves the source-label ambiguity for that saved executable. Inspection
of its matching source shows resource admission and protected model admission
preceding atomic publication of `launch-continue`; the Codex and Claude adapters
await admission before writing the request to the child's stdin. This contract
is conditional on the actual launch using that executable and protected launch
path. It does not create a historical process binding or establish that every
retained runtime used the same executable, policy and input path.

`1902-retained-audit-correlation-observation.json` records a separate read-only
inspection of the eight retained ownership records and their three referenced
audit logs. Each observed committed-prefix digest matched its unchanged anchor.
All 285 distinct recorded admitted resource paths were absent, but none of the
eight exact runtime names occurs in its referenced committed log. The canary
log has no admitted resource identity. Counts grouped by the preceding claim
are positional observations, not exact native-launch/resource correlation.

These observations do not close pre-admission lifetimes, delayed writer ingress,
unrecorded resources or the complete charged population. No audit was sealed or
cgroup removed. A producer must still recover the original runtime-to-execution
and resource binding, with a live exclusion against later ingress, before a
consumer can use original R closure. The source reproduction, a matching audit
digest, absent cgroup paths and absent runtime strings cannot be combined into
a serialized capacity permit. The alternative complete resource-bound route
and OT1–OT5 remain unresolved.

## Controller and journal obligations

The eventual mutation controller must have one root-owned, bounded registration
for the selected native root, owner UID, original store and supported kernel/
filesystem. Its administrative peer must be authenticated independently of
model output. Its IPC must bind the original operation, exact root/runtime,
live native-lock context and actual R capability; a copied response cannot be
converted back into that capability. The model/candidate receives no host
socket, descriptor, arbitrary-path operation, shell or flag-clearing authority.

Its intent must precede the first restriction. Intent, partial restrictions,
lost reply, helper death and a complete visible fence without complete resource
proof retain full demand. A committed receipt must bind both the verified fence
and the verified resource bound, not merely repeat the intent's proposed count.
Existing original native authority epochs and source/outcome histories remain
immutable. No initial un-fencing or receipt-retirement path is introduced.

These are required joins, not an implemented IPC, schema or privilege policy.
OT1–OT5 still block implementation handoff. The controller design must settle
its exact privilege/lifetime rules and durable schema against this boundary;
an independently reviewed small setter still cannot supply missing R proof.

## Consumer compatibility required before activation

The following inventory identifies concrete source joins, not claims that they
are implemented. All referenced paths are under
`crates/augmentagent-channel-core/src/` unless noted.

| Consumer | Current join | Required custody behavior |
| --- | --- | --- |
| `native_state_capacity::inspect_locked` | Counts retained directories and pending-transfer demand under a shared original state lock | Derive descriptive readiness from the same conservative evaluator as admission; no report becomes a grant. Missing/orphaned custody history remains charged. |
| `native_vm::Launch::prepare` | Runs `prune_and_count` before actual capacity admission | Validate the original resource bound and custody state before subtracting demand; retain original input/source/owner gates and fresh native construction. |
| `native_vm::prune_and_count` | Opens every session lock with `O_RDWR|O_CREAT`, even for retained counting | Recognize held/partial/incompatible custody before any write-open, marker test, alias deletion or pruning. Retain/count the session, and never infer terminality from a fence. Merely changing the lock to read-only does not authorize deletion. |
| `native_owner_handoff::session_lock` and handoff preparation/replay | Opens original session locks for writing before native inspection/transfer | Reject held, partial or unsupported custody before write-open or ownership mutation. Metadata observation cannot promote the held runtime into productive use. |
| `native_vm::retire`, `retire_owned_runtimes` and `reconcile_retired_runtimes` | Write markers and run original retirement/pruning | Preserve custody and unknown outcomes; ordinary terminal status or lease availability cannot override a held resource disposition. |
| Resume, alias registration and launch callbacks in `native_vm` | Reopen original ownership and publish aliases/outcomes | Revalidate custody before any mutation or model/input boundary. A refusal must retain the original lifetime/history, not fabricate successful cleanup. |
| Native owner outcome/preparation and workflow recovery readers | Consume original store, creation and protected observations | Resource credit supplies neither original outcome/nonrelease nor source/input/replay authority. Preserve their separate proof requirements. |
| Normal updater, rollback, CLI readiness and worker wakeup | Select running source/policy and schedule from capacity fingerprints | Report incompatible/unknown custody explicitly and keep full demand. A fingerprint change wakes revalidation; it does not grant capacity. Older builds must not bypass the physical fence or silently discard custody records. |

The old `O_RDWR` pruner is a concrete rollout blocker: even a valid visible
immutable lock would fail its open before fresh work reaches admission. This
must be fixed together with custody recognition and resource accounting, not by
loosening the physical fence. Supported old actors must be covered by the
privilege/resource contract; unsupported versions retain a conservative hold.

## Testable next exit

The next positive accounting test must use the real proposed R producer and
consumer. The open-unlinked writer above must either prevent construction of
the resource-bound capability or remain within an actual verified kernel bound
whose demand is still reserved. Rechecking the visible names, changing modes,
recording PID absence, returning a scripted success, ignoring the test or
retaining only a small metadata allowance cannot satisfy this test.

Then test lost replies, partial restrictions, original helper/descendant races,
orphaned journal/path identities, rollback/older consumers and credit
invalidation after a later actual admission. Preserve the original C/R and
productive-owner gates and the full CLI/Discord/shipment/soak acceptance. This
amendment does not redefine restoration around filesystem experiments.
