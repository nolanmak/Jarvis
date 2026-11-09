# Automatic capacity: planning audit record

Scope: publication of the C0 design task and C1-C5 blocked implementation tasks.
This is not implementation acceptance, GitHub code-review approval, a resource
grant or deployed restoration. C0 still owns exact equations, growth/lifetime,
observation, configuration/publication and reporting contracts.

Epic: [#1917](https://github.com/nolanmak/Jarvis/issues/1917). Contract task: [#1918](https://github.com/nolanmak/Jarvis/issues/1918). Documentation PR: [#1916](https://github.com/nolanmak/Jarvis/pull/1916).

Fresh-reader review plus 56 mapped seam checks across 19 focused review groups were completed. The author also checked conversation requirements against the document and verified source-backed findings. Targeted rechecks covered the corrected findings.

## Resolutions and tracked blockers

- S10: raw effective limits and net usable capacity are distinct; headroom is subtracted once (D2/glossary). Exact equations remain C0 work.
- S32/S34: N owns versioned resource/deficit payloads and uncertain-send reconciliation, including pending updates, visible uncertainty and bounded escalation (D9/C0). Detailed contracts block implementation.
- S15/S16/S40: retained commitments survive daemon/process exit; process-flock reclamation cannot release them (D4/D5). Positive retirement, native-growth producer and physical-enforcement limits remain explicit C0 blockers.
- S30: migration needs capacity/service configuration coverage in the existing retention-managed snapshot protocol, plus compatible rollback/roll-forward preserving security revocations/accounting/owner controls (D8/C3). Existing database/binary snapshots alone do not establish this coverage.
- S53: managed outputs include the aggregate runtime slice and service limits, with provenance. Planning excludes verified managed outputs; actual admission includes every enforced limit until reload/readback (D8).
- S33: public/private surface classification and field redaction remain C0-owned before reporting implementation.
- Fresh-reader findings on platform support, legacy override provenance and managed-limit bootstrap order are explicit in sections 1/D8/C0. Exact sizing and native growth remain design-task outputs, not decisions for downstream agents to invent.

## Seam coverage

| Mapped seams | Area checked | Result |
| --- | --- | --- |
| S1, S3, S7 | outer authority link; outer retained-runtime recovery link; implementation/deployment status terminology | Holds at planning scope |
| S2, S13, S20 | outer storage-ownership link; admission-owner handoff; native/shared-storage integration handoff | Holds at planning scope |
| S4, S28, S45 | amendment and product-mode decision; deployment/migration lifecycle; specification audit and publication handoff | Holds at planning scope |
| S5, S21, S48 | legacy recovery boundary and outer link; fixed overrides and rejected configuration; crash/uncertainty protocol across producers | Holds at planning scope |
| S6, S32, S34 | reporting integration and outer links; blocker/status payload handoff; private delivery lifecycle | Resolved or explicitly C0-blocked; rechecked |
| S8, S11, S39 | resource-dimension vocabulary; declared per-run demand handoff; run-resource/supervisor outer links | Holds at planning scope |
| S10, S14, S19 | protected headroom and usable-envelope decision; commitment vocabulary and preservation; full-growth-claim accounting contract | Resolved or explicitly C0-blocked; rechecked |
| S12, S24, S25 | observation versus execution authority; observation identity, freshness and coherence; unknown-evidence behavior | Holds at planning scope |
| S15, S16, S40 | release-proof handoff; accounting versus physical-growth guarantee; storage API side effects and lease lifetime | Resolved or explicitly C0-blocked; rechecked |
| S17, S18, S41 | filesystem identity and device accounting; duplicate-path claim accounting; build/snapshot consumer outer links | Holds at planning scope |
| S22, S23, S35 | shrink/pressure handling; recovery-to-scheduling handoff; fairness, holds and capacity deferral | Holds at planning scope |
| S26, S27, S44 | lock/revalidation protocol; cadence, hysteresis and event deduplication; task dependency and design-readiness gate | Holds at planning scope |
| S29, S30, S38 | durable publication and reload handoff; rollout/storage-control handoff; native-admission outer links | Resolved or explicitly C0-blocked; rechecked |
| S31, S33, S49 | current-worker effective readback; public/private information boundary; successful-shipment acceptance handoff | Privacy contract explicitly C0-blocked |
| S36, S42, S46 | fixed-default initializer outer links; incident-to-contract evidence boundary; test-evidence contract | Holds at planning scope |
| S43, S47, S50 | C0 ownership and consumer-contract handoff; managed-build outer link; non-goal scope boundary | Holds at planning scope |
| S9, S37, S53 | effective-limit computation; service-resource initializer outer link; managed leaf outputs versus enclosing-limit inputs | Resolved or explicitly C0-blocked; rechecked |
| S51, S52, S54 | supported-platform/profile boundary; unsupported activation versus temporarily unknown observation; managed-limit provenance and bootstrap/readback sequence | Holds at planning scope |
| S55, S56 | explicit scoped auto-migration operation; product authorization versus authenticated publication operation | Holds at planning scope |

Specification SHA-256: `37827a3c6b764c27687276f0e451290e2b33b4f31c4cf184d4af8cc42800e072`.

The detailed mapping and focused review notes were retained as session audit artifacts; this repository record preserves coverage and dispositions. Later contract or implementation changes require their own appropriate review and acceptance.

## C0 working checkpoint after issue publication

The separate [working contract](host-capacity-contract.md) now proposes exact
memory planning/admission equations and nine worked fixtures, and records the
source-verified native storage producer inventory. The planning audit above does
not approve that new contract or close #1918.

A fresh-reader checkpoint found one wrong-build ambiguity: Q could have been
charged against a daemon-only service ceiling for a worker launch. Section 2
now restricts that check to the new run's actual enforced ancestor chain, with
cross-service accounting at the real shared aggregate. Seven remaining findings
correspond to the contract's explicit open gates: storage, observation, other
resource dimensions, migration, runtime adaptation, rollback and reporting.
They remain owned by C0 and block downstream implementation. The reviewer found
the memory arithmetic internally consistent; the author independently recomputed
all nine numerical fixtures. This is arithmetic/design evidence, not runtime
test evidence. Full seam review is still required before implementation handoff.

The next C0 checkpoint specifies exact fractional CPU conversion, per-run
maximum versus reserved memory, desired/enforced reload boundaries, and a
memory/CPU observation protocol with conservative two-pass sampling, a 250 ms
collection bound, 500 ms freshness and two-attempt limit. Five CPU planning
fixtures, two reload-boundary cases and the observation boundary arithmetic
were recomputed. A fresh-reader recheck found no new wrong-build/forced-guess
findings in these sections. Shared supervisor scope, managed provenance and
real-entry-point replacement/shrink/cleanup evidence remain implementation
validation requirements; PID/FD and storage observations remain open C0 work.
This checkpoint is not the complete-contract seam audit or C0 closure.

The host task/descriptor checkpoint adds complete role declarations, full host
task reservations, effective cgroup/global/real-UID constraints, per-process
descriptor enforcement and a separately labelled system file-table pressure
check. Guest task/descriptor capabilities are not host reservations. The first
fresh read found two forced guesses: NPROC's process-local soft threshold versus
real-UID accounting, and the enabled role universe behind the task/memory ratio.
Both are resolved explicitly in P2/P3 and rechecked. The descriptor contract also
requires exact child limit/privilege readback and rejects inherited descriptors
outside that ceiling. No new wrong-build/forced-guess findings remained in the
recheck. Native-only role sizing, process-limit and pressure-boundary fixtures
were independently recomputed; this remains design evidence, not runtime tests.

Read-only deployed unit inspection at this checkpoint reported daemon/worker
task use 13/5, each TasksMax=2048, NOFILE soft/hard=4096 and NPROC soft/hard=52942.
It confirms the current separate service limits, not the proposed common
reservation accounting or its implementation. Nothing was published/reloaded.

The shared-storage checkpoint specifies one common admission owner, durable
commitments in each existing native database, a durable per-UID root registry,
per-filesystem full-demand arithmetic, pending/held/release-pending/released
lifetime, original-proof release ordering and create/resume/build/snapshot
fixtures. It includes the Python bridge's sparse-cache/command-storage consumers,
which do not currently publish Rust shared claims. The native build cache's
12 GiB demand and 20 GiB scratch floor remain distinct from retained 1 GiB
session accounting; auto mode must replace its managed 24 GiB aggregate default
without discarding explicit operator ceilings.

A fresh-reader checkpoint found the proposed arithmetic and lifetime handoffs
consistent. A continuation-pressure question led the author to require a
verified remaining-growth bound U, defaulting to the full original component
when unproven. Targeted recheck confirmed the corrected fixtures and that only
the qualified original writer may use U; every other writer counts the full
durable commitment. The author recomputed snapshot integer-byte rounding,
multi-root/device, concurrent-admission and continuation examples independently.

Final producer bounds, filesystem/identity encoding, SQL/replay/bridge APIs and
structural mixed-version/startup fencing remain C0 gates. This checkpoint is
not complete implementation readiness or a full-contract seam audit. The
separate executable/companion bound correction is implemented in PR #1925,
with local behavioral red/green evidence; it does not close these wider gates.

The lifecycle checkpoint separates configured mode, desired output, durable
publication, actual loaded enforcement and current admission. It defines uncertain
publication replay, resource-projection rollback preserving current authority,
and runtime adaptation using the existing unpaid reconciliation loop. A managed
increase needs two matching known observations at least 60 seconds apart; real
admission uses fresh locked evidence, shrink blocks new work immediately, and
ordinary contention recovery does not wait for another increase-stability cycle.
Unchanged resource deficits do not reopen productive hourly attempts.

The author checked these decisions against the updater's existing guarded
policy-only activation, worker upgrade watch and capped 300-second independent
reconciliation loop. Fresh-reader review found no new wrong-build/forced-guess
findings in L1-L7 and confirmed the timing/accounting examples. These are working
design fixtures, not implemented fake-clock/runtime test results. Final schemas,
step journal, compatibility/projection matrix and structural startup fence
remain load-bearing C0 gates before downstream handoff.

The reporting checkpoint defines CapacityStatusV1, typed predicate quantities,
explicit demand selection, bounded safe override provenance, legacy Progress
compatibility and unchanged health-array output. The author verified the current
Progress/outbox serialization, 1900-character message guard and health collector's
pruning side effect. Pure capacity inspection therefore uses a separate read-only
path; worker unpaid original-proof reconciliation remains allowed to clear
recoverable holds without waiting for a productive attempt.

Fresh-reader review found a real visibility gap: the first projection could not
label an active explicit override. N1/N5 now carry/render a bounded typed override
inventory, evidence completeness and safe provenance. Recheck found that resolved,
including root/session versus filesystem/PID distinctions. Additional author
checks preserved immutable dispatched content, separate workflow/capacity actions,
privacy redaction and attention limits across restart and repeated episodes.

The delivery checkpoint keeps later updates behind uncertainty, exposes local
attention after 10 minutes, and bounds private attention intents to one per six
hours/four per rolling day across episodes. Pending attention is not claimed
delivered and never determines execution or allowance. Final persisted attention
schema and #1714 integration remain explicit C0 gates. These are proposed
behavioral fixtures and reviewed working contracts, not implementation/live
acceptance or the final whole-contract seam audit.

The compatibility checkpoint adds S8/S9's proposed monotonic protocol floor,
same-inode negative open fences, staged old-writer handoff, and boot/rollback
requirements. In particular, a writer's already-open descriptor survives chmod;
new-client rejection cannot stand in for checked old-controller handoff. The
normal native-owner history contributes correlation, not cleanup authority.

The disposable Linux probe in `probes/storage_compat_permissions.py` passed
22 predicates across storage-temp and private systemd-user runtime locations:
12 permission/flock predicates and 10 actual legacy Python scratch-gate
predicates. It records the imported bridge SHA-256; at this checkpoint that is
`42bbc1aa57cbc2e750f9ee804132aee0799adedeb0233857974676b39e390c6f`.
It creates only private temporary files and never creates a VM image, opens an
application state/lock, invokes a model, or changes production permissions.
The mode-01700 scratch-root discriminator rejects the legacy allocator's exact
0700 validator, while a preopened root descriptor remains usable. This is
evidence for that negative gate, not a complete migration test or a grant.

Additional source tracing includes the Rust compute-retention `usage.json`
publisher consumed by Python scratch admission. It must be covered by the final
consumer/startup matrix. Scratch-root mode alone does not fence that allocator
or every sweeper. New protocol session paths avoid the legacy immediate-child
session-prefix sweep; original live legacy paths must not be moved.

The compatibility audit identified eight already-open C0 implementation-contract
gates: original handoff producer/completeness, full bootstrap entry-point matrix,
protocol/marker/registry formats, per-step journal/crash recovery, snapshot
projection/compatibility matrix, actual growth/inode/quota profiles, held bridge
APIs, and exact SQL/config/provenance/legacy-versus-bound migration handlers.
These remain prerequisites for implementation handoff, not follow-up cleanup
after activation. The checkpoint does not close C0 or unblock C1-C5.

A fresh-reader recheck found a boot-specific ambiguity: fence recreation could
be read as enough to reopen admission after an interrupted cutover. S9 now
explicitly resumes the same journal and requires checked handoff/import/
publication/loaded completion before admission. The probe now refuses an
unexpected legacy-source SHA and executes the exact hashed bytes, and the
contract explicitly limits temporary/runtime observations instead of presenting
them as certification of production filesystem profiles. Final pinned legacy
entry-point and full migration/profile tests remain required.

The quota-observation checkpoint (O7) distinguishes read-only Q_GETFMT success,
ESRCH inactivity and unknown errors, with both-pass coherence and complete-vector
requirements. Source verification against the Linux quota API/kernel showed
Q_GETFMT does not require special quota privilege. The non-root x86-64 probe
returned ESRCH on all 12 user/group/project queries across the current state,
build, store and archive devices. Separate mount inspection identified ext4;
the command/input order and per-descriptor device IDs preserve traceability.
No quota, policy, accounting or production file was changed. These are current
CLI samples, not worker grants or a complete storage observation implementation.

Fresh-reader review found no new material contradiction in the checkpoint.
Two evidence-polish points were resolved: the probe says non-root rather than
claiming full absence of capabilities, and records device IDs alongside the
documented input order. Exact active-quota profiles, protected typed encoding,
filesystem identity/support, image/inode/durable balances and complete collector/
admission integration remain explicit C0 gates. An inactive quota result cannot
be used to manufacture numeric unlimited capacity or ignore another predicate.

The filesystem checkpoint (O8) separates persisted full ext4 UUID identity from
current device/mount/namespace/object attachment. It specifies held-path
revalidation, single-pool alias arithmetic, collision handling, reboot accounting
rebinding and descriptor-based available-byte/inode counter units. A UUID match
never supplies source provenance, native custody, execution or cleanup authority.
The inspector remains separate from admission's mutating CI-hold reconciliation.

The non-root filesystem probe read nonzero 16-byte UUIDs and held-descriptor
counters on the four current devices without exposing UUIDs, reading raw block
devices or invoking SET/mount operations. Three disposable predicates confirm
held-directory stability after replacement, reopened replacement inequality and
same-filesystem identity despite distinct directory inodes. Pairwise comparisons
also distinguish the four input devices without collision and recognize a
repeated repository input. This is not bind-mount, actual worker-namespace,
statx-attachment, publication-boundary, reboot or complete migration acceptance.

Fresh-reader review found no new hard contradiction. It confirmed that complete
global inventory revision semantics, descriptor-bearing constructor interfaces,
root/database/anchor encoding, write profiles and the rest of storage admission
remain load-bearing C0 gates. Its experiment-polish suggestion to emit pairwise
input comparisons was implemented; these comparisons do not enlarge the stated
evidence scope. No production configuration/accounting was modified, and C1-C5
remain blocked pending complete C0 closure and the final whole-contract audit.

The bounded memory component in `host-memory-facts-contract.md` received a
fresh-reader audit, author conversation/source cross-check and all 16 mapped
seam checks (six grouped read-only auditors). No retained component-contract
finding remained. This closes that small implementation contract, not full C0.
The feature plan explicitly permits these settled data helpers and strengthening
the existing supervisor decoder under C1 while complete discovery, accounting,
migration, activation and remaining C0 work retain their prerequisites. This
removes an overbroad helper dependency without weakening any execution gate.

The existing `available_memory()` source accepts the second token on the first
matching line without validating total, units, duplicates or u64 range. TDD
fixtures in the implementation worktree reproduce 11 malformed observations
accepted through that unchanged production entry on main `416f392b`; valid
kB/zero and missing-availability compatibility cases run separately. Missing
new parser/planner APIs are not counted as deployed behavioral regressions.
Implementation/CI/independent review/rollout evidence remains separate.

The memory-admission refinement separates the protected managed-envelope share
H from the existing runtime free-memory margin F_mem=256 MiB. H still reduces
the managed maximum once; current OS/unrelated use is accounted by MemAvailable
and ancestor charges. The revised busy-host, aggregate, external-pressure,
explicit-reserve, equality and one-byte-deficit fixtures were recomputed with
exact rational/integer arithmetic. The proposal adds the runtime margin to its
external-ancestor predicate; it does not change the deployed fixed-mode policy
or the closed M1-M3 helper component in PR #1929.

Fresh-reader plus author source/conversation checks and 24 mapped amendment
seams were reviewed in eight groups. Two substantive findings were resolved:

- Maximizing aggregate charged memory and then subtracting it from reservations
  can minimize future slack and falsely fit a new run. M6/O3 now derive R_i,
  C_i and S_i in each complete pass and retain upper R and upper S separately.
  The decreasing-charge fixture exposes the formerly false 8.25 GiB fit and
  requires 10.25 GiB against 9 GiB available. Root-added seam 25 covers the same
  task-accounting correction in P3/O6 and its typed N2 projection: the task
  counterexample correctly refuses 2008 required against 2000.
- A protected-reserve minimum could not be represented by N1's old ceiling-only
  entry. L1/N1 now retain typed property/value/provenance, distinguish maximum
  from protected-memory-reserve, and render the latter as an outside-capacity
  minimum. Equal numeric H/B values do not merge these meanings.

Rechecks found both findings resolved, including the additional task seam and
fresh-reader equality-boundary suggestion. All 25 scoped seams hold at this
checkpoint. This is not a full working-contract handoff audit, code approval or
current-worker admission evidence. Existing complete-vector, common accounting,
observation/provenance, SQL/migration/rollback, original handoff and projection
integration gates remain open. No production setting, reservation, retained
session, model call or shipping allowance was changed by these design checks.

The allocator-fence checkpoint corrects the earlier 01700 feasibility limit:
Rust compute retention masks permissions with 0777, so a sticky bit alone does
not reject it. S9's revised candidate fences both scratch and compute authority
roots using mode 01710 plus an exact access ACL (owner rwx, group none, mask x,
other none), with no named/default ACL. It preserves effective owner-only access
while both pinned legacy permission predicates reject the new root format.
ACL publication precedes sticky chmod; exact intermediate/final readback is
required. Configured authority-root paths remain fenced; v2 child selection is
internal, and old data paths are not moved.

`probes/storage_compat_acl.py /tmp /mnt/build /mnt/store` passed 24 narrow checks
across three private disposable roots, including intermediate 0710/ACL readback,
final 01710/ACL readback, actual old Python rejection before allocation and
owner descriptor writes remaining possible. The script pins/executes the exact
hashed bridge bytes and pins the Rust retention source. Its Rust result is a
source-predicate evaluation, explicitly not a compiled Rust invocation. Effective
non-owner denial follows the checked ACL and documented kernel algorithm; no
other-UID impersonation test was run. No image or production permission changed.

Fresh-reader/source checkpoint review found no new feasibility overclaim. It
confirmed that retained `Lease::retain/discard` descriptors and artifact exports
bypass fresh root validation, so original handoff and complete consumer/startup
coverage still govern migration. Final import must account for existing caches,
compute usage and task records without omission/duplication. Compiled Rust/new
client acceptance, actual profile integration, SQL/journal formats and the
positive handoff producer remain explicit C0 gates. The optional intermediate
readback improvement was implemented and the disposable experiment rerun.

Separate read-only source tracing of #1758 at `49535c2e` confirmed that owner
metadata recovery returns guarded-setup readiness and has no productive caller
for `inspect_owner_bootstrap`. Scope materialization retains descriptive original
creation handles, not complete all-writer custody. The ACL experiment supplies
neither productive source/resource/input qualification nor a replacement grant;
that constructor and normal owner-ingress work remain required under #1703.


## Shared-authority extraction prerequisite (#1930)

The closed shared-storage-extraction-contract.md was checked by a fresh reader,
conversation/source comparison, and all 28 mapped sub-seams grouped across five
mechanism boundaries and crosscut delivery/persistence obligations. Source
counterparts were inspected at unchanged main 416f392b. One source-bound finding
was resolved and rechecked: active CI lifetime locks protect their source but
are skipped by global admission reconciliation; abandoned/unlocked unresolved
holds block admission. No remaining scoped contract finding was retained.

Source verification confirmed the real core CommandAdmission dependency is
acyclic, Busy re-exports preserve downcasts, the checkout allocation split is
feasible, and current tests comprise 11 holds, four reservations and three
checkouts. Test movement must keep one actual registry and fake binding under
cfg(test), update the crash-child name, and positively verify child execution.
The extraction keeps formats/namespace/lifetimes; native durable integration,
full C0 handoff, code approval, deployment and #1703 acceptance remain open.

Implementation PR #1931 at b4a77220 follows this scoped contract. The managed
unchanged-main baseline passed 11 cleanup cases; the exact-head combined
core/CLI run passed 41 selected cases, including all 18 moved tests and actual
CLI consumers. Crash children verify an execution marker plus original holds.
The worktree is clean and whitespace checks pass. Hosted targeted/exporter
setup fails before tests on pinned release download HTTP 504s; remaining CI,
independent code approval, merge and rollout are unfinished. No production
policy/session/service changes or native enrollment occurred.


## Native durable balance/replay checkpoint

The new native-storage-ledger-contract.md and executable SQL were checked by a
fresh reader, conversation/source comparison, and all 16 mapped seams. Six
findings were resolved and rechecked: initial terminal insertion, retained
filesystem placement, new-resume versus historical validity, retained release
ordering, global session binding, and SQL REPLACE bypass. The final DDL has
pending-only insertion and collision guards covering all unique keys, plus
retained-state filesystem and launch-settlement constraints. No remaining
checkpoint finding was retained. This is not a full C0 handoff audit.

The committed disposable Python/SQLite experiment passes 41 predicates on
SQLite 3.45.1, including real constraints/transitions, replacement refusal,
checked overflow, missing graph/replay refusal, full create/resume/retirement
history, read lock refusal, reopen and pre/postcommit process crashes. Both
children provide verified execution markers. It touches only its own temporary
SQLite fixtures; it exercises neither production Rust admission nor original
release capability, filesystem/quota/inode or kernel power-loss behavior.

The native existing retirement table remains unchanged in the proposed schema.
Original binding/proof APIs, registry/anchor/schema migration integration,
full growth and profile enforcement, and bounded pre-intent control-plane
reservation/transfer/recovery still block production implementation. In
particular, ledger and rollback-journal writes cannot be authorized by their
own not-yet-committed row. These obligations remain in #1918/#1920; no DB,
registry, permission, snapshot or deployed capacity policy was changed.


## Shared extraction hosted verification

At source head b4a77220, all 25 reported hosted checks/statuses on #1931 pass.
The initial pinned Deno/Python HTTP 504 setup failures recovered after direct
checks of the same assets returned HTTP 200. Exporter passed its failed-job
retry. Targeted's second attempt passed setup but correctly refused the still-red
exporter; after exporter recovered, its third attempt passed the full dependency
verification. No source rewrite or check bypass was used. Independent accepted
approval, merge and normal rollout remain outstanding; CodeRabbit success is
not accepted approval. This does not enroll native storage or activate sizing.


## Bounded native initrd producer contract (#1932)

Fresh-reader and all 17 mapped seams were checked against main 416f392b's
native boot_entries/main and shared newc encoder. Two findings were corrected
and rechecked: the current final seed can be 65 MiB pending separate #1927,
so the new 128 MiB archive cap is one total ceiling with no 64+64 guarantee;
held-source reads must reject observed shrink/short EOF as well as growth.
The closed component bounds encoded data, not whole-launch physical growth,
aggregate memory, original custody, durable release or activation.

Tests were defined first in jarvis-native-initrd-budget. Four additive-contract
cases reproduce three missing native refusal assertions plus absence of the
new shared codec argument. This is not a claimed old enforced-limit defect.
Production-path/per-category, source mutation/nonregular, output failure,
compatibility and full suites remain required implementation evidence. No VM,
credentials, model, production policy or services were changed.

The scoped implementation is now #1933 at c60a7ae1. All 13 new component cases
pass; existing native 20, provisioning 9, shared VM 11 passed/6 skipped, CI VM
16 and CI-contract 23 give 92 passing cases in total. Six existing KVM runtime
cases were not run, and no VM/model/credential deployment proof is claimed.
Actual native main calls bounded encoding before QEMU; suite registration is in
the shared CI command authority. Host raw archive and compressed logical file
bounds do not establish whole-launch physical allocation, aggregate RAM,
original C/R input/body authority, durable storage or restoration. Hosted CI,
independent code approval, merge and rollout remain pending.


## Native publication temporary-storage contract (#1934)

A real unchanged-main experiment reproduced four leaked 64 KiB temporary files
on four failed file-sync publications (262144 bytes), without changing the old
destination. Two test-first regressions reproduce file-sync and rename leakage.
Fresh-reader and all 13 mapped seams were checked against native atomic,
RefreshCheckpoint, main, outer startup handling and gateway counterparts.

Resolved/rechecked findings specify original-error preservation when main tries
its failure observation, fixed OSError-subclass uncertainty and sanitized exit
126 despite refused startup diagnostics, exact source evidence after a raised
rename, explicit complete short-write handling, and the remaining pre-existing
1 MiB credential-output/read round-trip gate. No checkpoint/digest clears the
new process latch or authorizes native release. Full metadata/growth, original
custody, schema/migration and productive handoff remain C0/#1703 gates.

Implementation PR #1935 at cef4663d fixes the original publication leak and
conservatively fences uncertain writes. There are 69 passing local cases,
including all 15 new storage/publication/actual diagnostic-path faults. No skips,
VM/model, real credentials or production state changes. The existing hosted
native suite contains these tests without another CI registry edit. The stale
export fixture now uses the actual production-private credential-parent mode.
Hosted verification, independent approval, merge and rollout remain pending.

The remaining credential round-trip gate is now #1936, with a synthetic actual
begin/sent/commit/read reproduction on main: 900150 bytes → 1162285 bytes,
committed journal, refused by the 1048576-byte reader. No live issuer was called.
The future complete producer profile also must inventory direct relay copying,
Rust helper/spec materialization, interpreter bytecode caches, bounded audit/
anchor growth and all relevant publication files. These are C0 growth obligations,
not silently assumed covered by the fixed retained-session charge or these
atomic/boot/archive component bounds.


## Bounded archive hosted verification

All 32 reported hosted checks/statuses on #1933 pass at c60a7ae1. The hosted
native suite includes all 13 new archive cases. Accepted independent approval,
merge and normal rollout remain pending; a CodeRabbit status alone is not
approval. This is archive-component evidence, not original productive ingress,
complete capacity integration or #1703 deployed acceptance.

## Credential round-trip contract (#1936)

The scoped native-credential-roundtrip-contract.md closes a demonstrated refresh
writer/reader mismatch. Fresh-reader and all 17 mapped seams were checked;
UTF-8/Unicode scalar requirements and response-envelope versus constructed-output
ordering findings were resolved and rechecked. This authorizes only that narrow
producer/consumer repair under C0, not full integration or activation. Initial
behavioral RED reproduces oversized Claude and Codex commits, unreadable Unicode
export and a committed marker skipping bounded current-file validation. No live
credential, issuer, policy or service was exercised.

## Credential round-trip delivery / publication hosted checks

PR #1937 at cd6f34af implements #1936's scoped repair. Behavioral RED reproduces
both provider oversize commits, Unicode expansion, committed validation bypass
and Rust's one-byte-over usable declaration. Local GREEN: native 29, gateway 16,
I/O 9, provisioning 9 and managed Rust readiness 7 = 70 passing, no skips/ignored
cases. CI contract/whitespace pass. Fault fixtures cover all four checkpoint
writes for both providers; actual gateway success-header ordering is exercised
with scripted transport. Hosted CI is running. Independent accepted approval,
merge and normal rollout remain pending; no live issuer/VM/credential/state or
policy change occurred. This closes no full capacity/productive ingress gate.

The earlier atomic publication repair #1935 at cef4663d now has all 17 reported
hosted checks/statuses passing, with no submitted reviews. This is hosted CI
proof, not independent accepted approval, merge, rollout or live acceptance.

## Credential repair required privacy recovery

#1937's first hosted bridge suite passed, but required privacy found a generic
API-key match on adjacent public OAuth route/client-UUID fixture literals. Head
b7b3f15b separates those assignments; no scanner rule/allowlist or production
code changed. The SHA-256-verified pinned gitleaks 8.30.1 scanner passes its exact
git-archived release tree; all 29 native cases reran green. Hosted bridge-suites,
privacy and node-release now pass at b7b3f15b (runs 42595275735/42595275315).
Other hosted jobs remain running with no current failures. Independent accepted
review, merge and normal rollout remain pending. Capacity integration and
productive owner ingress remain unfinished; no live policy/state changed.


## Publication replay checkpoint and actual crash evidence

host-capacity-publication-contract.md narrows L2/L3's durable multi-file mechanics,
with an explicit remaining-gate list and no implementation exception. The actual
embedded provisioner was killed after real replace 1/2/3 in disposable state; all
three fsynced execution markers and SIGKILL statuses were checked. The first two
leave mixed candidate/source files, all precede manager reload, and ordinary
same-source reruns complete. See 1703-capacity-publication-crash-proof.json and
probes/native_capacity_publication_crash.py; no future publisher/physical power
loss/native custody/production activation proof is claimed.

Fresh-reader review identified discarded installed-object evidence after prefix
advancement. J4/J5 now persist completed_objects atomically with the prefix and
retain them through recovery/last-loaded evidence; J10 adds the crash/replacement
fixture. Recheck resolved the finding. All 53 seams are inventoried, not claimed
reviewed. Whole C0/full seam review, final schemas, writer/startup handoff, security
supersession, control-plane growth, snapshot and process receipt contracts still
block integration and activation.

## Audit-anchor byte producer contract (#1940)

The audit-anchor-budget-contract.md component closes the anchor writer/reader
size mismatch without changing 64 MiB limits or authority. Actual managed
AnchoredAudit append/seal regressions fail on unchanged main 416f392b; all five
existing audit-chain tests pass. The log-sized escaped payload and seal callback
produce anchors the current reader would refuse. Fresh-reader and all 19 mapped
narrow seams were checked; one already-sealed retry ambiguity was resolved and
rechecked. Separate #1918 implementation exception is required. This is neither
full physical/memory growth nor production restoration evidence.

## Audit-anchor implementation and production CLI attempt

PR #1941 at 5cc066f1 implements #1940's reviewed narrow serialization bound.
Managed protected-audit consumers pass 47 cases, no failures, one existing live
Codex/public-web case intentionally ignored. Six new tests cover actual append/
seal failure, exact/+1, ordinary temporary/original-object preservation, actual
short/zero/failing/cumulative writes, prior pending recovery and already-sealed
proof behavior. A broad fixture assertion initially matched payload_sha256; the
full rerun passed after checking only the actual record field. CI contract and
whitespace pass. Hosted CI, independent code approval and rollout remain pending.
This encoded-file bound does not establish complete physical or memory growth.

#1937 at b7b3f15b now has all 24 reported hosted checks/statuses passing. The
production autopr run command accepted PR 1937 with its durable request ID;
the worker claimed it and made four recorded model calls. Its first attempt
failed at acceptance_registration_retry, before review/merge, because generated
verifier roundtrip-bound failed its own candidate-size precondition on the
baseline. See 1703-pr1937-command-proof.json for the bounded receipt. Token
telemetry is zero/unknown, not a claim of zero model usage. No shipping credit or
deployment is confirmed, and no benchmark/capacity-policy change was made.

Source tracing separates two follow-ups: #1942 under adoption #1537 covers
fresh issue dedup that sees only agent-fix branch heads despite authorized linked
PRs; #1943 under retry #1706 covers invalid independent verifier repair/bounded
recovery. Existing adoption and one baseline correction already exist; neither
is falsely described as absent. Those real failures must be repaired through the
normal authority/review/CI path. Full capacity integration, productive owner
ingress and #1703 CLI/Discord/merge/fault/soak acceptance remain incomplete.

## Initial acceptance repair contract (#1943)

acceptance-verifier-repair-contract.md is separate workflow-repair scope, not a
capacity C0 exception. Fresh-reader and all 31 narrow mapped seams were checked;
missing-evidence exit/environment identity/reset ordering/positive-persistence
crash findings were resolved/rechecked. Source 2ba400d2 preserves the existing
one-correction behavior in the actual preparation path and its managed CLI RED
fails because a second independent correction is unreachable. The closed scoped
contract allows implementation while complete capacity/owner/live requirements
remain unfinished. Code approval/hosted CI/normal rollout are still required.

## Initial verifier repair draft implementation

Draft #1944 at 932e8807 implements #1943's reviewed workflow coordinator. Rejected
records use stable epoch/slot keys, exact proposal/predecessor identity and the
existing immutable store; known failures use cache-only proposal lookup. The
current executor alone supplies positive evidence. Fresh/resume/source-less
callers preserve typed holds; narrow progress projection codes retain them after
command completion. Tests include real store reopen, original-process crash
boundaries, missing committed responses, strict lineage, hold idempotence and
public progress; passing results remain unproven pending hosted validation.

Local managed RED at 2ba400d2 reproduced the one-correction dead end. The first
implementation compile caught a fixture field mismatch, corrected to evidence_hash.
A subsequent managed run waited on the actual worker VM's disk leases; only the
exact own waiting manage-parent process was cancelled before compilation. No VM,
cache, limit or production control was changed to bypass admission. Hosted CI at
the current draft head is running; CI-contract/whitespace pass. Independent code
approval, rollout and original-command live recovery remain pending. #1941's
separate audit-bound patch now has all 16 reported hosted checks/statuses passing,
without code approval or rollout. Full capacity/owner/live acceptance remains open.

## Linked-PR duplicate scoping investigation

#1942 now has a repository [investigation contract](linked-pr-issue-dedup-contract.md).
Source verification confirms both primary and remote branch-only guards, the
existing arbitrary-branch adoption path, targeted issue reconciliation, and the
publication intent/direct-identity recovery boundary. The draft requires shared
source facts, complete inventory, no paid duplicate scope and race-boundary tests.
Strict trust/unknown classification, remote dispatch, pending intent exclusions
and the external-create race remain explicit handoff gates. No implementation,
RED/GREEN, approval, deployment or restored shipment is claimed.

## Verified verifier-repair CI and normal command submission

#1944 at 932e8807058b667f165350109d9a22c8936f3127 now has all 16
reported hosted checks/statuses passing. The downloaded targeted log confirms
all nine new coordinator tests, original-process crash/reopen, missing committed
response detection, three-source-family hold and command-finish progress tests.
Full CLI binary suite: 1,753 passed, zero failed, 33 existing ignored cases.
The later redundant local managed run was still waiting on the actual worker VM
lease; only our own exact waiting manage-parent was canceled after hosted success,
and its original handle terminated 143. No local GREEN or VM/cache change claimed.

The PR is now ready for independent code review. The actual production command
accepted request autoship-recovery-pr1944-20261109 through the existing guarded
autopr API after a successful target-validation dry run. The
[receipt](1703-pr1944-command-proof.json) records queued, worker/review/merge/
deployment unproven. No review, CI or merge bypass; no restart of PR1937.
The separate #1942 guard regressions are in draft #1945 at 97444937, with hosted
execution pending and no behavior repair or confirmed RED yet. Complete capacity,
productive owner ingress and parent #1703 live acceptance remain unfinished.

## Linked-PR suppression design audit

The [#1942 contract](linked-pr-issue-dedup-contract.md) now closes strict
trust/unknown classification, complete outer inventory and nested-reference
saturation, both remote dispatch/publication paths, pending exact-identity
reconciliation, precise holds and external-create race limits. Installed gh's
pinned source confirms that complete outer PR listing does not hydrate nested
closing-reference pages; saturated trusted connections must be unknown.

Fresh-reader, conversation/source comparison and all 26 mapped narrow seams were
checked. Red-main's existing safe Resume return was clarified; first-hold timing
was made exact; an actual oversized-scope catch-all that could relabel a held
parent was explicitly covered with typed propagation and regression requirements.
Affected seams and historical observation provenance were rechecked. All 26
scoped seams hold. This permits only the scoped #1942 repair after original RED,
not independent code approval or whole capacity/owner/rollout completion.

Draft #1945 at remote 97444937 still awaits original hosted regression execution.
Local test-only commits also cover the actual primary/remote issue selectors and
require positive recognition for valid fixtures; they remain unpushed while the
original immutable-head CI run is live. No behavior fix or passing Rust result
is claimed. Original active issue1943 and queued PR1944 remain distinct retained
workflows; the [receipt](1703-linked-pr-dedup-observation.json) records chronology
and limitations without inferring productive progress from heartbeats.

## Confirmed linked-PR guard RED

The original #1945 hosted run at 97444937 terminated with exactly the three
expected guard regression failures: primary arbitrary-branch claim, remote claim
and malformed JSON. Full CLI result was 1,741 passed, three failed, 33 ignored;
no unrelated failures. The [RED receipt](1703-linked-pr-dedup-red.json) binds the
exact head, run/job, failure names and downloaded-log digest. This is actual
behavior failure, not missing new API or source-string inspection.

The stronger c162f2a1 baseline was pushed only after that original job finished;
its actual primary/remote selector execution remains pending. A local draft now
shares adoption references, checks complete authorized inventory and wires typed
holds into fresh model/decomposition/publication boundaries and status. The
managed local test run acquired its registered build lease and is compiling, so
its source is frozen until the original handle terminates. No passing Rust,
independent code approval, merge, activation or restoration claim.

## Linked-PR draft implementation and selector RED

#1945 at ede8a63a implements the shared reference observer, both complete
inventory adapters, strict metadata/reference validation, model/decomposition/
publication checks, approved-remote-run preservation and precise existing-stage
holds/progress. Original actual-selector baseline c162f2a1 completed with the
five expected failures, 1,741 other passes and 33 ignored cases.

The first local draft compiled and passed eight focused cases; two new guard
fixtures lacked synthetic origin/main. Corrected fixtures then passed while a
new malformed-reference URL test gave actual RED (22 pass/one fail). After URL
validation, 23 related cases passed, including real primary/remote selectors,
midflight guard, child-create hold, immutable admitted-plan/candidate preservation,
unchanged-stage hold idempotence and command-finish progress. The privacy hook
then flagged a synthetic email-shaped userinfo URL; the test assembles identical
input from fragments and the hook passes without bypass. Final-head full CLI/
hosted tests remain pending. Broader publication/remote-approved race fixtures,
pre-guard command status, independent review and rollout remain delivery gates.

Formal closing associations on #1944→#1943 and #1945→#1942 are now verified.
They previously only said Refs in prose; the earlier #1943/#1944 overlap alone
was not a proven structured claim. Existing eight component PR/issue pairs were
also inspected and already had correct formal links. No heads/request IDs changed.

A normal foreground work-once attempt on existing queued PR1944 refused before
dispatch with execution-readiness-blocked/native-runtime:policy-missing in this
interactive shell. Status remains queued, no last attempt/active lease. This
does not diagnose the configured background worker's policy; no environment was
copied or guard bypassed. The original active issue1943 later showed eight calls
and acceptance_failed Build, a real change from its earlier seven-call stage.
Queue acceptance, model work and focused tests remain distinct from shipment.
