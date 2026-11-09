# Capacity publication replay checkpoint

Working C0 amendment under #1918/#1921/#1917/#1703. This specifies the durable
multi-file publication boundary in L2/L3. It is not full C0 closure, a new resource
allocator, a productive execution grant or an implementation exception. Full
configuration/provenance, original migration handoff, bounded control-plane growth,
snapshot projection and current-process readback remain prerequisites below.

## Source boundary and reason

The current `scripts/ensure-autopr-worker.sh` prepares daemon/worker/slice files,
keeps previous bytes only in memory, publishes them sequentially, and attempts
rollback on a caught error. Process death loses that plan. The current
`scripts/check-for-updates.sh::reconcile_native_selection` uses ordinary deployment
and restart guards, while `autoship_deployment.rs` and
`scripts/autoship-deployment-readiness.py` already own process-bound receipts.
Extend those authorities; a second provisioning or restart loop is not allowed.
The legacy single-file command in #1913 does not establish this multi-file replay.

## Immutable operation

J1. Keep one active capacity publication for the selected deployment. The updater,
installer, enable-auto command, runtime reconciler and compatible resource rollback
all use the same exclusive publication gate. An unchanged unfinished operation
is reconciled, never replaced with a newly computed candidate. That gate and all
non-resource writers of an affected file must be integrated before implementation
handoff; the existing in-memory rollback is not a substitute.

Persist an immutable operation body and a separately versioned progress record
in the existing private effective-configuration state domain. The exact path and
lock-inode migration must join the final startup/compatibility contract; they are
not permission to choose another root on contention or missing state. One active
operation plus the last loaded receipt may be present. Terminal history may be
compacted to bounded receipts, but an old request cannot become a new operation:
its expected predecessor revision must still match, and unknown old IDs refuse.

J2. Operation body fields are exactly:

| Field | Type and meaning |
| --- | --- |
| `version` | Integer 1; unknown versions refuse |
| `operation` | 32 lowercase hexadecimal UUID digits |
| `deployment` | Original deployment UUID, same encoding |
| `kind` | `install`, `enable-auto`, `adapt`, or `resource-rollback` |
| `expected_revision` | Integer 0..2^63-2, predecessor resource configuration revision |
| `expected_configuration_sha` | 64 lowercase SHA-256 hex digits |
| `candidate_revision` | Exactly expected_revision+1 |
| `candidate_projection_sha` | Digest of the immutable resource-only candidate |
| `observation_sha` | Digest of the qualified planning observation, descriptive |
| `demand_sha` | Digest of the complete declared role/run/storage profile |
| `provenance_sha` | Digest of the classified managed/explicit property inventory |
| `snapshot` | Existing retained snapshot reference and projection digest |
| `compatibility_sha` | Digest of the exact supported binary/schema/protocol matrix |
| `files` | Ordered array of 1..16 file plans, described below |

All digests identify exact canonical payloads, not fresh execution, native release,
provider authorization or permission reconstructed from JSON. UUID collisions,
changed payloads under an operation ID, arithmetic overflow and missing referenced
artifacts refuse. IDs/digests alone never excuse the current guarded checks.

Canonical operation bytes are UTF-8 JSON with fixed ASCII field names sorted at
every object, no insignificant whitespace, strict Unicode scalar strings,
integers in decimal, no floats/duplicate keys/nonfinite values. Null is permitted
only for the explicitly absent source in J3. Hash these exact bytes. Limit the
encoded body to 64 MiB before any publication; actual read length is bounded too.
Do not truncate a plan to make it fit. Original body digest remains immutable;
progress never changes the candidate or expected predecessor.

J3. Each ordered file plan has exactly `target`, `parent_binding`, `source`,
`candidate`, `resource_projection_sha` and `nonresource_sha`:

- `target` is a fixed managed destination selector resolved by the trusted
  provisioning code for this deployment, never a journal-supplied path. Supported
  selectors are the selected native policy, shared native selection, daemon
  resource unit fragment, worker unit, aggregate runtime slice, shared scratch
  resource configuration and capacity mode/provenance configuration. There is at
  most one of each; unrelated files, executable pins and arbitrary drop-ins refuse.
- `parent_binding` records the qualified existing parent namespace identity used
  by the original publisher. The final filesystem/root identity encoding and
  supported reboot rebinding come from O8/S8, not a path string comparison.
- `source` is null only for a verified originally absent target; otherwise it
  contains observed original object identity, full-file digest and exact mode.
  Unknown or symlink/special/multilink/unsafe ownership is not absence.
- `candidate` contains exact bounded bytes as canonical base64, digest and mode.
  Decoded bytes are at most 2 MiB per file. Policy/selection/private state use
  0600; unit/slice outputs use the existing provisioning mode, fixed in the final
  producer schema, not an arbitrary mode accepted from this journal.
- `resource_projection_sha` identifies only the intended owned resource fields.
  `nonresource_sha` binds every other field/content property of the source and
  candidate. A trusted format-specific renderer must prove preservation before
  preparation and again under the publication gate. A whole-file digest alone
  cannot prove that current grants or credentials were preserved.

The format-specific renderer, mode values and exact allowlist of owned resource
properties are remaining C0 gates. No wildcard selector or generic text patch is
a fallback. All affected existing producers, including native runtime refresh,
worker provisioning and operator resource mutation, must participate in the
same gate before this plan may authorize a write. Security changes never become
stale candidate fields to replay blindly; see J8.

## Progress and physical write sequence

J4. Progress contains exactly version=1, operation/body digest, monotonic integer
sequence 0..2^63-1, state, completed prefix length, `completed_objects`, current
step or null, and a bounded fixed reason code. Progress is at most 65536 encoded
bytes; validate the actual bounded read and strict field types before use.
`completed_objects` has exactly one installed original object identity, candidate
digest and parent-binding reference for each file in the completed prefix, in
file-plan order. No duplicate, skipped or extra index is valid. Retain these
identities through recovery and the last-loaded receipt; a prefix count cannot
replace them. The current step identifies one file index and phase:
`creating`, `writing`, `replace-pending`, or `replace-observed`. It retains the
original private temporary leaf/object identity after creation, candidate digest,
and any observed installed-object identity. These fields are descriptive recovery
facts; they are never a native ownership or resource-release capability.

Use the L3 states `prepared`, `publishing`, `publication-uncertain`,
`published-awaiting-load`, `loaded`, `failed-before-write`. Immutable body is
persisted and synced before prepared progress. Persist publishing/current-step
intent before creating any publication temporary. Reconcile an uncertain progress
record against the same body; a lost progress-write acknowledgment cannot start
another candidate. A missing/corrupt body or progress is unknown and fences new
auto admission. Preserve all existing claims/holds/accounting.

J5. For each file in the fixed original order:

1. Under the common publication gate, revalidate current authorization,
   compatibility, namespace, expected original/candidate prefix and absence of
   unrelated source changes. Revalidate the actual guarded updater boundary.
2. Persist/sync current-step `creating` before creating one exclusive private
   temporary relative to the held parent. Capture original FD identity. Persist
   `writing` and that identity before payload writes. Failure between creation
   and identity persistence leaves an unqualified temporary: hold uncertainty;
   do not infer cleanup authority from its name or sweep it.
3. Write the exact bounded candidate with checked complete writes, revalidate the
   original temporary and parent, sync its content, then persist/sync
   `replace-pending`. No target mutation precedes this record.
4. Revalidate source/non-resource/namespace/current guards. Replace relative to
   the same held parent. Revalidate the installed original temporary identity;
   persist `replace-observed` with that identity, sync the parent, then persist
   the installed identity into completed_objects, advance the prefix and clear
   current step in one atomic progress replacement. Do not discard evidence for
   an already completed target. Recheck the expected prior progress digest and
   increment sequence exactly once; lost acknowledgment reconciles that same
   transition rather than forking another progress history.
5. A lost rename acknowledgment or failed parent/progress sync becomes uncertain.
   Do not restore the old target, unlink an unexplained object, or claim untouched
   failure. Any cleanup needs the original exact unpublished temporary proof;
   uncertain ownership remains held under the bounded growth contract.

Publication records and temporary overlaps themselves require pre-existing
qualified control-plane storage admission; the not-yet-committed operation cannot
authorize its own allocation. The body cap is an encoded bound, not a claim that
64 MiB is the complete physical peak or that the allocation bootstrap is solved.

J6. Completed prefix is an observation of writes, not durable current truth.
Every reconciliation inspects all targets and the current original temporary
against that same operation under the publication gate:

| Observed state | Result |
| --- | --- |
| Every target is the recorded original source and no target change can have occurred | May close failed-before-write after settling precisely owned temporaries |
| Exact candidate prefix, exact remaining source suffix, current step consistent | Resume that guarded step of the same operation |
| Every target is the exact original candidate object/content with synchronized parents | published-awaiting-load |
| Source/candidate bytes match but namespace or required original identity does not | publication-uncertain |
| Missing/replaced object, malformed plan/progress, unexplained mixed set, changed non-resource fields | publication-uncertain |

A crash before a rename can leave replace-pending with the original temporary
and original source; resume the same rename after revalidation. After rename,
matching the recorded temporary's installed identity and candidate bytes permits
parent synchronization and advancement, even when the old completed prefix has
not advanced. Mere candidate-byte equality without the required namespace and
original-object evidence is insufficient. Unsupported reboot/identity recovery
remains unknown under O8/S8; no fresh operation or copied receipt cures it.

J7. After every candidate file is durably installed, request ordinary guarded
reload through the updater. No publication step invokes a direct paid workflow
or forces an active worker restart. published-awaiting-load persists until both
actual serving roles record the same operation/candidate revision and the complete
aggregate and service kernel limits are freshly read back under O1-O8. Extend the
existing process-bound receipt and inspector, not a separate cached enabled flag.
A legacy receipt, missing birth/boot binding, daemon-only restart, different
worker policy or desired-but-unloaded increase cannot mark loaded. Receipt and
projection extension schema remains a C0 gate; old receipts remain valid for their
old schema but cannot establish new capacity activation.

J8. An unrelated/security change at any point stops resource replay before another
target write. Preserve the new security state and the original publication record;
never restore the old whole policy or fabricate failed-before-write after a
partial publication. Exact authorized reconciliation/supersession of such a mixed
state is an explicit remaining C0 gate owned by P/R. Until it is specified and
tested, this checkpoint does not authorize implementation of the publisher.
Normal runtime refresh is included in that writer inventory; bypassing its gate
would make a whole-file preservation check racy. This limitation is deliberate,
not permission to indefinitely block revocations behind a resource operation.

## Snapshot and rollback boundary

J9. Use only `augmentagent deploy snapshot` and existing retention. Extend its
manifest with a bounded versioned resource projection and its digest before the
snapshot can qualify this operation. Add snapshot references from the active
capacity operation to the existing retention reference collector; do not create
ad hoc backup directories or permanent pins. Unknown reference evidence retains
the snapshot under existing policy. Snapshot refusal leaves migration unstarted.
The exact projection fields, artifact size/peak and safe reference retirement are
remaining P/E gates. Current database/binary-only snapshots do not satisfy them.

Rollback is a new resource-only operation checked against current configuration,
commitments and supported protocols. It never restores saved owner/accounting/
credential/security data. A snapshot for fixed 8 GiB cannot override current
9 GiB commitments. An older binary ignoring durable balances refuses regardless
of numeric fit. No rollback uninstalls a monotonic storage fence.

## Required vectors and implementation gate

J10. Before implementation, define behavior fixtures through actual provisioning,
publication, updater, receipt and snapshot entry points with disposable state:

- Exact plan/body limits, invalid/duplicate fields, changed operation payload,
  stale predecessor, alternate target selector, overflow and unknown schema.
- Crash before and after every progress write, temp creation/identity write,
  payload write/sync, rename, parent sync and completed-prefix publication.
- Original source, exact candidate prefix, all candidate, same bytes/replaced
  inode, changed parent, malformed/missing temporary and lost rename acknowledgment.
  Crash immediately after prefix advancement: retained completed_objects must
  distinguish the original installed inode from a same-bytes replacement.
- A security/resource change before preparation, between files and while a failed
  operation is pending; preserve current non-resource state in every outcome.
- Daemon reloaded/worker old, reused PID/new birth, reboot, process receipt old
  schema, complete kernel readback, active-work/pin/memory/restart guard refusal.
- Snapshot cap refusal, missing projection, active operation reference retention,
  old protocol rollback, smaller-than-commitments rollback and compatible overlay.

These are requirements, not tests reported as passed. A disposable model probe may
check the replay state table, but cannot stand in for original object custody,
physical sync, kernel/process receipt, updater or snapshot integration evidence.

## Open threads and handoff

- P/R: exact compatible mode/provenance/native policy schemas and owned-property
  renderer; closes the configuration/override part of C0 before C3 coding.
- P/R: one existing authority path/lock and participation of every affected writer,
  plus original migration/startup handoff; blocks publication implementation.
- P/R: J8 security-preserving partial-operation reconciliation/supersession;
  blocks publication implementation, not permission to weaken current revocation.
- R: pre-intent control-plane reservation, full physical growth and precisely
  owned temporary recovery; blocks creation of the body/progress/temp artifacts.
- P/E: snapshot projection/reference lifecycle and compatibility matrix;
  blocks migration/rollback, with no retention-policy override.
- P/H: exact process receipt extension and inspector mapping; blocks loaded claims.
- E: full fresh-reader/seam audit and real crash/integration fixtures; blocks handoff.

The carrying cost of these parked threads is that no publisher or activation may
be implemented from this checkpoint. It records specific replay mechanics so the
remaining contracts can close without silently inventing a second allocator or
restoring stale authority. Full C0/C2/C3, productive owner ingress and #1703's
CLI/Discord/autonomous shipment/fault/soak acceptance remain unfinished.

## Checkpoint evidence and audit scope

[Disposable actual-provisioner probe](probes/native_capacity_publication_crash.py)
executes the unchanged embedded Python body from ensure-autopr-worker.sh, SHA-256
`dd870acb177f678ddd8fc50d1d1c317abd37f9f8e5f14c1fe7309a1ebab40e73`.
[Recorded results](1703-capacity-publication-crash-proof.json) verify actual
SIGKILL after real replacement 1, 2 and 3 using fsynced child execution markers.
The first two leave candidate/source mixtures, and none reached manager reload.
Ordinary same-source reruns completed all three cases. The shell Linux/cwd wrapper
is not exercised by the injected-body run; normal rerun uses the real shell
script. Systemctl and release executables are disposable scripted fixtures.
No host units, production state, VM or model are touched; physical power loss,
future durable replay, original native custody and activation are not proved.

Fresh-reader review found one internal false join: clearing current_step after
advancing the prefix would discard the only installed candidate-object identity
needed by J6. J4/J5 now retain completed_objects through recovery/last-loaded
readback and atomically publish them with prefix advancement; J10 includes the
post-prefix crash and same-bytes replacement fixture. The finding was rechecked
as resolved. Conversation/source comparison preserves the owner's automatic
host-sizing requirement and normal rollout constraints.

The independent map inventories 53 seams; mapping is not a claim that all 53
have passed narrow seam audits. That complete audit and the named Open threads
remain required before implementation handoff. This checkpoint grants no C3
implementation exception or capacity activation.
