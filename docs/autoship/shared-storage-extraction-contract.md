# Shared storage authority extraction

Status: audited scoped prerequisite #1930 under #1920/#1918/#1917/#1703.
Full C0/C2 and automatic sizing remain open. This document is the complete
contract for a behavior-preserving extraction only, not native enrollment.

## 1. Decisions and implementation boundary

D1. Move the existing CLI reservation implementation and CI cleanup-hold
implementation into augmentagent-channel-core. One shared implementation owns
Admission, Busy, Reservation, Claims and CI Hold. CLI modules re-export these
same types, including Busy identity used for exit code 75. No copied allocator,
optional reconciliation callback, second lock namespace or alternative native
entry point is permitted. Every Admission::lock continues to acquire the
existing global flock then reconcile original CI cleanup holds before returning.

D2. Extract the existing storage_leases::lock_directory implementation into
channel-core storage_coordination; CLI target leases call that same helper.
Preserve /run/user/UID selection, /tmp fallback, directory name, UID/mode/type/
symlink checks and file formats exactly. Keep target lease implementation and
CLI disk dependencies in the CLI. This does not change logout/reboot lifetime.

D3. Move OwnedCheckout and allocate from self_improve_verifier_checkout into
channel-core storage_checkout, because the cleanup integration test must exercise
the same registry as Admission and Hold. Keep async Git export, registered
contract checks, Python harness selection and watch_reserve_roots in the CLI.
The CLI re-exports OwnedCheckout and allocate. Preserve the existing JSON
identity encoding/hash, stable paths, private_json validation, source lock,
ownership checks, cleanup failure behavior and explicit unlock on Drop.
Do not change the source-export authority or accept a serialized replacement
for the original resource admission. Fields remain private; public access is
path(), allocate() and the existing production hold operations only.

D4. Move the existing checkout ownership and cleanup tests alongside that
implementation. Core tests use one cfg(test)-only registry fixture from the
actual shared hold module; no production feature, environment selector,
configurable registry path, duplicated module/TLS or exported fake proof API.
The original hold fixture's fake resource binding remains cfg(test) and
crate-private. The actual Hold::bind still accepts the original CommandAdmission.
Update the crash-test child executable's exact test name to the new module;
the parent must verify the child actually ran one test, rather than treating
an executable with zero matches as success.

D5. Reconciliation remains inside the shared authority and uses the existing
positive original resource/host/boot/kernel proof. Preserve live lifetime locks,
prepared/admitted/uncertain states, bounded private reads, protected-directory
identities, atomic fsynced records, original receipts and failure retention.
Live lifetime locks continue to be skipped by admission reconciliation: active
holds protect their source but do not block all unrelated allocations. An
abandoned/unlocked hold with unresolved proof blocks admission.
Hold cleanup grants no CI success, merge authority or native custody proof.
Keep reconciliation internal to the crate; expose only operations required by
existing CLI consumers. Existing fixture functions remain test-only.

D6. Reservation semantics are unchanged: v1 claim encoding, UUID names,
per-device deduplication, checked ownership, current stale-claim handling,
exported descriptor lifetime and current arithmetic. Do not add native durable
balances, new claim versions or migration markers in this extraction. The
known v1 unlocked-record-before-version behavior stays a separately tracked
migration constraint; exposing this API does not authorize durable native use.

## 2. Test-first delivery and acceptance

1. Record baseline absence of the shared API as an architectural dependency,
   not a deployed behavioral regression. Build storage-focused existing tests
   against unchanged main using the managed wrapper in a named worktree.
2. Preserve/move the existing tests for cross-state claims, per-device
   deduplication, snapshot kind, exported reservation lifetime, prepared and
   uncertain CI holds, positive original kernel proof, crash/restart recovery,
   source preservation/reallocation and inherited checkout descriptors.
3. Add a positive child-test sentinel: each exact-name crash helper must write
   its original fixture result or emit a marker verified by the parent, so
   renaming a test cannot silently erase restart coverage.
4. Verify real shared Admission refuses an abandoned/unlocked unresolved hold
   after original controller drop/crash, then admits after original cleanup
   proof, with source preserved until settlement. A live hold continues to
   protect its own checkout without blocking unrelated admission; preserve the
   current live-lock skip. These are existing behavior assertions executed
   against the moved authority.
5. Run focused core reservation/hold/checkout tests and relevant CLI build
   storage/verifier suites through scripts/cargo-managed.sh. Compile the actual
   CLI consumers, including error downcasts and CI CommandAdmission binding.
   Run whitespace/source-diff checks and required hosted CI at the tested SHA.
6. Independent code approval and ordinary merge/updater flow remain required.
   Record exact baseline/new SHAs, test counts and skips; a contract audit is
   not code approval. No production capacity policy or services change here.

## 3. Seams to audit before implementation

- S1: CLI adapters versus core type identity and global lock namespace (D1-D2).
- S2: Admission lock versus CI proof/reconciliation and lifetime locks (D1,D5).
- S3: checkout allocation/drop versus hold registry and original source ownership
  (D3,D5).
- S4: cross-crate test movement versus production authority and crash-child
  execution (D3-D4, tests 2-4).
- S5: extracted v1 claim API versus future durable native accounting (D6).

## 4. Deferrals and remaining gates

Native durable balance SQL, original native writer handoff, shared root
membership, full growth/inode/quota profiles, legacy fencing, migration,
automatic sizing activation and live acceptance remain owned by #1918/#1920
and their dependent issues. They block native enrollment, not the unchanged
existing CLI behavior extracted here. The public API is not promised stable
across future internal migrations. This extraction stores no new records or
new fact needed for those later migrations and makes no reboot-survival claim.

Audit: fresh-reader and 28 mapped sub-seam checks, including source counterpart
verification at main 416f392b, are complete. One finding was resolved/rechecked:
live holds protect their checkout but are skipped by admission reconciliation;
only abandoned/unlocked unknown holds block global admission. No remaining
contract finding was retained. This scoped exception permits #1930 only; it
does not waive full C0/C2 dependencies or independent code review/CI. Full
#1703 productive owner ingress and live acceptance remain required.

Implementation evidence is pending. The unchanged-main managed baseline is
running in jarvis-shared-storage-authority; no extraction code is written yet.
