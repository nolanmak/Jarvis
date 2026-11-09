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

## Linked-PR repair final hosted verification and delivery check

#1945 at c5c91de999a7cf4d9525d76dd1e3cb50014a0bdf has all 16 reported
hosted checks/statuses passing. The exact-head log confirms all 1,759 CLI unit
tests pass, zero failed, 33 existing ignored; all 21 submission integration tests
pass. New pre-checkpoint CLI/private blockers and authorization priority, four
pending-publication modes, three approved-remote modes and the actual oversized
scope-refusal consumer are individually confirmed. The latter now owns the
real parent mutation fallback, so the fixture proves a child-create claim returns
before any parent comment/label rather than only testing a transport helper.

The prior ede8a63a full CLI suite already passed 1,755 cases; its CLI integration
step found a minimal-row fixture becoming unknown with an untyped exit 4. The
shared fixed command blocker vocabulary now preserves deferred unknown/claim
status before a checkpoint exists, with valid/unknown/fork/ambiguous CLI cases.
The following test-only mutable-store compile error was corrected. Managed full
local attempts ended at storage-busy admission without compilation/execution;
final broad validation is hosted. No cache, live VM, limit or deployment bypass.

The PR is ready for independent review and its normal production command was
accepted under autoship-recovery-pr1945-20261109. The
[command/CI receipt](1703-pr1945-command-proof.json) distinguishes queued from
execution, review, merge and deployment. The original #1937/#1944 requests and
active issue1943 were not restarted or reset. CodeRabbit success and no-secret
Claude no-op are not accepted code review; normal rollout remains pending.

The production delivery CLI verified the configured private owner DM. Its default
was already correct; explicit bindings for queued PR1944 and active issue1943
were accepted. A read-only join verifies confirmed actual send receipts against
those routes; the [delivery proof](1703-private-progress-delivery-proof.json)
includes no private identities/content. Null latest pending sequence with no
pending rows is not absence of progress: actual state/confirmed sequence are
read separately. This establishes transport confirmation, not owner initiation,
reading, productive coding follow-up, merge or deployed restoration. Complete
capacity/owner integration and #1703 acceptance remain open.

## Live command starvation during an active builder

[Read-only production evidence](1703-active-build-queue-starvation-proof.json)
records PR #1944 queued for over two hours and #1945 for over 27 minutes, both
without a command attempt. The independent worker is live; issue #1943 retains
a fresh generation-1 lease and active VM descendants. This is not a proven hang.

Source inspection of `AutoPrLoop::run_builds` establishes that queue polling
occurs only during the wait between attempts. Its active `tick()` select polls
shutdown and the tick, with no command queue branch. `run_selected` separately
holds a global execution lock throughout the attempt. Simply adding another
queue poll or starting another worker cannot establish safe peer execution.
Independent merge reconciliation already exists but cannot supply the missing
review/execution attempt for these queued PRs.

This is evidence for the existing #1706 ready-peer requirement, not a new capacity
policy or an assertion that spare full-run capacity was available. Its fix needs
a behavioral regression with a progressing long-lived builder, an authorized
queued PR, explicitly sufficient declared resources and a ten-minute bound. Also
verify insufficient capacity yields the precise blocker without model spend;
no duplicate target execution; original work/lease custody survives; owner pause
and shutdown retain their semantics; upgrades do not cancel an active builder;
and allowance remains based only on confirmed merges. Test the real command
selection/execution boundary, rather than source strings or callback count alone.
The implementation must explicitly resolve the global-lock handoff using existing
workspace and resource authorities before enabling concurrent execution. No
whole-run timeout, VM interruption, second supervisor or original-work reset was
applied. This records a live reproduction; it does not claim RED/GREEN test or
implementation completion.

## Queued PR lane source audit and first behavioral regression

[Working command-lane contract](command-pr-lane-contract.md) reuses the existing
resume lane rather than discarding a fresh issue continuation. Fresh-reader,
conversation/source and all nine mapped seams were checked. Actual source
revealed controller-path provenance drift, unflocked shared attempt-history
writes, PR result misclassification/paid-held fairness omission, and an untyped
source identity error. They explicitly block concurrent consumer activation.

#1946 is a native child of #1706. Draft PR #1947 adds three production-worker
fake-clock regressions at tests-only eb7b0304. Two behavioral failures are expected
on unchanged settlement, but the hosted run has not yet proved that RED. The
fixtures inspect durable FairSched, DailyCounter and merge allowance after one
actual scheduler turn, distinguishing paid PR/build holds from unpaid holds.
This bounded test-first repair does not enable concurrency or claim autosizing,
independent approval, merge, deployment or live queue recovery.

## Normal pause requested for duplicate issue1943 work

[Control receipt](1703-duplicate1943-pause-proof.json) records the normal CLI
join (`autoship-control-duplicate1943-20261109`, accepted/joined) and subsequent
accepted pause revision 1 on the original issue1943 workflow. Direct pause first
refused because autonomous work had no durable command job; joining the existing
workflow supplied the supported control entry without resetting its generation.
PR1944 and PR1945 have no shared alias and remain unpaused.

The original lease was still active at observation, with ten recorded admissions.
Enforcement and descendant teardown are unverified; no process was killed, no
lease cleared and no quiescence/released-resource claim is made. This stops
future work only when the existing controller enforces the pause. It is not a
worker restart, a repair deployment or a confirmed merge.

## Paid-held fairness: behavioral RED verified and fix pushed

[Exact-head proof](1703-paid-held-fairness-red-proof.json) records tests-only
eb7b0304, run 42624703591/job129185362626: exactly the paid PR/build hold
regressions failed with stale persisted (2,3) debt; 1,742 CLI unit tests passed
and 33 existing tests were ignored. The unpaid-hold control passed. Both cases
reached the fairness assertion after verifying one turn, compute settlement
and untouched merge allowance. This is behavioral RED, not a compile failure.

PR1947 now contains implementation 3eb3f586: billed fairness is settled once
before the held/idle exit, preserving existing compute/confirmed-merge
accounting. Whitespace and Rust syntax parsing pass. Hosted GREEN run
42626678480 is live; no passing result, independent approval, merge or rollout
is claimed yet. Original issue1943 still retains its lease after the accepted
normal pause, and queued PR1944/1945 remain unpaused and unclaimed.

## Recovery command advances; shared history repair reaches real RED

PR1944's [original command proof](1703-pr1944-command-proof.json) now includes
its selected durable attempt, active generation-1 lease, five model admissions
and Verify checkpoint at local candidate ca50608c. Its published head remained
932e8807 at the observation, so old remote CI does not prove the new candidate.
The duplicate issue1943 managed processes previously observed are no longer in
the worker tree; no native-retirement credit or full quiescence is inferred.

The paid-held fix's first implementation run passed all three new behavior
cases but failed one obsolete source-layout assertion (1,743 passes/one failure,
33 existing ignored). PR1947 head1ff444fc retains rotation checks and replaces
only the accounting-layout assertion with completed-paid worker controls. Five
worker tests now cover six paid/deferred/unpaid cases. Full hosted validation
42629214521 remains live; this is not a passing full suite.

#1948 is a native child of #1706; draft #1949 tests-only9a3589e1 produced
[three real writer lock violations](1703-attempt-history-red-proof.json), with
1,741 other CLI unit passes and 33 existing ignores. Implementation388e6202
routes all three writers through one original-path lock before reading through
replacement, with six behavior/failure/crash tests. Hosted42630481781 is live;
independent approval, merge and rollout remain pending. No consumer is enabled.

[Source-only holds](1703-repair-issue-holds-proof.json) on issue1946/1948 prevent
the older selector duplicating their active draft PRs while the dedup repair is
unmerged. They were admitted/paused through the normal CLI and have no shared
PR alias at observation. Do not resume them while their PR remains active.
Full capacity, provenance compatibility, targeted PR classification, typed
identity outcomes and parent live acceptance remain unfinished.

## Repair prerequisites GREEN; filtered-source defect reproduced

PR1947's [paid-held fairness proof](1703-paid-held-fairness-red-proof.json)
now records exact-head 1ff444fc GREEN: 1,746 CLI unit passes, zero failures,
33 existing ignored. PR1949's [three-writer proof](1703-attempt-history-red-proof.json)
records 388e6202 GREEN: 1,747 passes, zero failures, 33 existing ignored,
including all six new lock/failure/crash tests. Both PRs are ready and admitted
through their original normal CLI requests; neither is merged or deployed.

The live worker actually selected the queued repairs. Its authorization refusal
was [GitHub quota exhaustion](1703-worker-github-quota-proof.json), not proof
of revoked grants. This misleading classification and its retry behavior are
recorded under #1706. An interactive dry run later succeeded; that does not
prove the worker's token/API quota bucket recovered. No credentials changed.

#1950/#1951 repair a separate real source-input defect: credential filtering
omits .env.example while the CLI required it at compile time. The same actual
production BuildSnapshot/production Rust module case first failed missing input,
then passed at 2691add3 with a public 147-name/no-values source asset.
The [bounded proof](1703-filtered-env-input-proof.json) records both heads,
run/job identities, canonical public data and the limits of local/hosted evidence.
Actual CLI tests and remaining hosted checks are still running; draft1951 is
not yet admitted. Source issue1950 is held through normal CLI controls to avoid
a duplicate build, with no native resource release inferred.

PR1944's original attempt completed after eleven recorded admissions and
approximately 99.7 minutes, publishing head3c00c759. It now has no active lease
and is held for resume_preserved_work. Old932e8807 CI does not cover that head.
No command reset, manual candidate edit, code approval, autonomous merge,
capacity activation or completion of #1703 is claimed.

## Missing integration coverage corrected; exact review hold identified

The completed 2691add3 CLI job passed 1,741 unit tests with 33 existing ignores,
but inspection of its actual log showed the full workflow's explicit suite list
did not run the two new env inventory integrations. #1951 final04c29b8d adds
that test target to existing rust/cli/6 and extends its registration check.
Contract verification, 23 contract tests and 11 targeted-plan tests pass locally.
Hosted bridge42645634641 is live and Rust42645634622 queued at observation;
no final-head or actual-integration GREEN is claimed yet.

PR1944's specific [independent review hold](1703-pr1944-review-dependency-proof.json)
is now tracked in native child #1952 under #1709: the requested immutable
self_improve.rs at3c00c759 is 967,956 bytes, above the 524,288-byte one-request
recovery bound. Existing initial evidence sharding does not solve that requested
whole-file recovery path. The new issue records behavior-first exits and
load-bearing design gates; it is not implementation-ready or permission to
truncate source, raise limits or substitute CI for approval. Its issue lane is
held through normal CLI control pending that design/implementation handoff.

The worker later passed authorization and claimed original PR1945, with an
active generation-1 lease and five recorded native admissions at observation.
This proves recovery beyond the earlier quota refusal, not completed acceptance
or merge. Original requests, candidate/provenance and all parent live exits
remain preserved.

## Review dependency design audit and PR1945 actual failed attempt

The [review dependency contract investigation](review-dependency-recovery-contract.md)
records a fresh-reader rejection of the multipart-local-approval proposal:
complete bytes distributed among reviewers do not prove anyone reasoned about
a dependency crossing parts. Five additional findings concern encoded bounds,
wake/counter identities, unknown original-result recovery, response persistence
and failure transitions. All six are open design gates; none is code approval.
A map enumerated 30 seams for that proposal; they are not all checked. The
replacement direction must investigate the existing final-authority immutable
window selector and preserve actual coherent review, unread-source labels and
all budget/provenance/diagnostic semantics before handoff.

PR1945's [original command proof](1703-pr1945-command-proof.json) now records
its actual finished attempt after seven model admissions and approximately
19.8 minutes. The independent baseline verifier failed to compile its generated
fixture: E0277, PathBuf formatted as Display. That is unexpected verifier
failure, not intended behavior RED. The source repair remains #1943/#1944; no
original request reset or manual protected-verifier edit was performed.

#1951 remains draft at04c29b8d. Its final bridge regression job is GREEN;
the explicitly registered actual CLI integrations and other required checks
are still awaiting the original hosted jobs. No main rollout or autonomous
merge is claimed.

## Filtered-source repair ready and queued after actual CLI GREEN

#1951 final04c29b8d now has actual CLI job129258197660 GREEN: 1,741 unit
passes, zero failures, 33 existing ignores. Both newly registered integration
cases individually passed: runtime-template precedence and standalone fallback
with exact canonical parity/regeneration. Final bridge42645634641/job129258010808
passes the fresh filtered-module compile; Python suite502 passes/53 skips.
The [proof](1703-filtered-env-input-proof.json) retains logs/hashes and the
earlier registration gap without crediting it as integration GREEN.

The PR was marked ready and admitted via normal CLI request
autoship-recovery-pr1951-20261109, joined=false, canonical independent workflow
nolanmak/jarvis/pr/1951. Readback is awaiting-worker-claim, no last attempt,
no active lease and no owner pause. Source issue1950 remains separately held.
Other required Linux jobs and aggregate targeted verdict are still running;
queue admission is not execution, approval, merge or rollout. No source/body
churn is planned while this request owns the published candidate. Parent
automatic capacity, productive ingress, live shipment and soak exits remain open.

## Scoped coherent-source design closed; native fixture/tool mismatch confirmed

The [C1 range/item contract](review-dependency-recovery-contract.md) is ready
for test-first implementation after [fresh/source/all32 seam checks](1703-review-range-design-audit.json).
Three fresh findings were resolved, one actual-incident-input gap correctly
parked until the preserved command emits an actual typed dependency request.
One seam correction documents the existing episode-evidence persistence hook.
C2 failed-review history, free deterministic holds and wake semantics remain
required and unready; no code approval or live recovery is claimed.

#1951's original worker attempt finished after six admissions/~8.2 minutes:
two baseline observers differ as intended, but the filtered-module Python
observer cannot find rustc and the CLI observer assumes a binary not built.
Source inspection confirms only cargo verifier invocations receive the trusted
Rust toolchain; Python PATH is fixed system-only. This is a fixture/tool-plan
mismatch owned by #1943, not the intended product bug and not permission to
expose ambient tools. Original candidate/request/native custody are preserved.

Its remaining hosted legacy-python job failed because the runner lost server
communication. GitHub supplied that annotation, no failing-test step and no
job log. Only that job was rerun against unchanged04c29b8d; accepted/queued
at observation. The [proof](1703-filtered-env-input-proof.json) separates actual
CLI/bridge GREEN, infrastructure retry and failed native acceptance. No
required check, review gate or normal updater was bypassed.

## Coherent-source C1 enters real test-first implementation

Native child #1953 under #1952 owns the reviewed C1 implementation. Draft
#1954 at9146acb4 extracts the existing production group adapter without changing
the old recovery algorithm and adds actual Git/Rust/scripted-transport cases.
The large fixture compiles/runs to confirm8 for the seeded cap defect and3
when fixed before the mock review sees source. Two new cases are expected
RED; the legacy control retains an initial concrete finding after approval.
[Checkpoint proof](1703-review-range-test-first-proof.json) records the pending
hosted run42657183954; no automated RED/GREEN is claimed before actual results.

The managed local attempt waited at parent admission with only its original
bash/augmentagent processes and no compiler. Only that owned waiting group
was interrupted; exit130 and original PID absence were verified before edits.
No Cargo cache was borrowed/force-unlocked or created outside registration.
Verification moved to immutable hosted CI. Source issue1953 is separately
held through normal CLI while its manual draft is developed.

#1951's infrastructure retry and aggregate recheck are now GREEN on unchanged
04c29b8d (Rust run42645634622 attempt2/job129287129560; targeted42645634722
attempt2). All currently reported statuses/checks pass, but its first actual
native attempt still failed acceptance registration from unsupported fixture
tool/binary assumptions. No accepted independent review, merge, main rollout
or parent live recovery is inferred from CI success.

## Coherent-source actual RED and implementation checkpoint

The original tests-first head9146acb4 completed with1742 CLI unit passes,
two intended coherent-source failures and33 existing ignored cases. The
legacy retained-finding control passed. Headb4605e45 attempt2 adds the
protected-path failure:1742 passed,3 failed,33 ignored. Its first attempt
failed during Deno installation before tests; an unrelated code-mode
integration assertion also failed on attempt2. Neither is counted as
source-recovery RED. Exact run/job and log hash are in the updated proof.

Draft1954 now contains implementation11495e1f: strict typed requests, immutable
item/byte lookup, coherent recovery, source-coordinate packing and bounded
observations/cache encoding. Rustfmt syntax parsing and whitespace checks
pass; hosted compile/tests are pending. Native incident replay, independent
review, merge, normal rollout and C2 remain open. This is no capacity
activation or autoship restoration claim.

## Coherent-source compilation correction

Implementation11495e1f run42665671159/job129328536696 failed compilation:
the diagnostic helper lacked caller_plan, and three old regressions called
the removed reader. This is neither behavioral RED nor GREEN. Head7d865307
shares production/diagnostic caller selection and runs those original
regressions through the new immutable source producer. It also preserves
inner container attributes and adds correction/identity/wire-boundary and
large-stream EOF tests. Hosted run42667732148 is pending; no native acceptance,
review, merge or rollout claimed.

## Coherent-source C1 hosted GREEN; final scope correction pending

Head7d865307 run42667732148/job129335771012 passed the hosted targeted
crate tests and applicable Linux aggregation. CLI unit result:1754 passed,
0 failed,33 existing ignored. The seeded cross-item defect/fixed cases,
legacy retention, protected paths, exact selector limits, correction bound,
metadata/native IDs, Unicode coordinates, container attributes and large
immutable byte/EOF cases pass. The proof retains the exact head/job/log hash.

Final revision7812314e corrects the expanded recovery prompt to remain partial:
the initial inventory can exclude separately streamed caller groups, so fitting
that inventory does not establish global source coverage. All evidence is still
supplied and all controller groups/roles remain required. The existing real
recovery test now asserts that label. This final head is pushed but not yet
validated;7d865307 GREEN is not credited to it. Draft1954, independent approval,
normal rollout, original1944 replay, C2 and full1703 acceptance remain open.

## Scope-corrected C1 GREEN and final boundary test head

Head7812314e run42670493609/job129345442448 passes1754 CLI unit tests,
0 failed,33 existing ignored, including the expanded partial-scope assertion.
The same hosted job remained intact during a shared GitHub quota exhaustion;
observation resumed after its declared18:20:19 UTC reset and confirmed success.
No expired observation was treated as a stopped build or reason to restart.

Final test-only head3debdb8d adds the contract's explicit exact/+1 serialized
request/response/raw-byte and8MiB writer boundaries, plus an actual-adapter
case requiring every same-name item to fit before another observation. It is
pushed; run42673227287 is pending. This head has no GREEN claim yet.

Normal CLI dry-run validated PR1954 with exit0/reason target-validated; accepted
was false as expected for a dry-run, and the PR workflow/event counts stayed
0/0. This does not queue work or prove native readiness. Independent review,
normal merge/updater, original1944 replay, C2 and full1703 exits stay open.

## Final-head hosted startup failure and same-head retry

3debdb8d targeted run42673227287 attempt1 failed before creating a Rust job.
Its public workflow annotation reports Internal server error (correlation
e79cd831-5aa9-48ab-8e23-74438438bff7). Five other failed workflows on the head
report their own matching workflow-level server errors; their source URLs
are in the proof. None is credited as a Rust test failure, RED or GREEN.

The six failed workflows were each confirmed terminal on the same head before
normal rerun requests were accepted. Targeted attempt2 is authoritatively
queued on that unchanged head; no observation timeout caused a restart, no
source changed and no required check was skipped. Final validation remains
pending;1754-test GREEN on7812314e remains correctly scoped to that head.

## Final C1 code tests GREEN; required Linux gate remains held

On final head3debdb8d, targeted attempt2/job129350959940 completed the real
crate-test step successfully:1758 CLI unit tests passed,0 failed,33 existing
ignored. All four explicit exact-boundary/no-extra-observation additions pass.
The full job failed only in its later applicable-Linux aggregation: exporter,
bridge-suites and codex-redirect were missing from finished runs whose public
workflow annotations report Internal server error. No failed test is inferred.

GitHub's official status reports a major Actions outage and an investigating
Actions/Checks API incident beginning18:12:37 UTC. We stop blind retries, keep
all required checks, and record the actual job/log hash in the proof. Code
GREEN is not full CI GREEN. Ready-for-review/normal submission can proceed
with this hold explicit; no review, merge, updater rollout or incident exit is
claimed. The epic1703 checkpoint now links the actual capacity/review issues
and current PR heads, preserving every original completion requirement.

## C1 ready, normal command accepted, worker preflight deferred

PR1954 on3debdb8d is ready for review. Normal CLI request
autoship-recovery-pr1954-20261109 was accepted with stage queued,
complete=false and until=merged. Actual private progress sequence1 has
a confirmed receipt on the currently bound route. The [command proof](1703-pr1954-command-proof.json)
records the later authoritative worker status and receipt counts without
publishing private identifiers.

The worker attempted preflight and recorded execution-readiness-blocked, with
no native model admission events. A durable command job exists even though
an executing workflow row has not been instantiated. A proof writer initially
assumed such a row must already exist; it failed before writing, then normal
CLI status and command-job/attempt records established the correct pre-admission
state. The request was not lost or resubmitted. Its precise underlying readiness
cause is not established by the generic status or an operator-shell probe.

Ready/queued follows final code-test GREEN with the required CI outage hold
explicit; it is not merge permission. No Linux/privacy/node-release or
independent-review gate was waived. Preserve this command/PR input and the
source1953 hold. C2, original1944 replay, automatic capacity and all full1703
acceptance remain unfinished.

### PR1954 deferred readiness: configured-worker capacity observation

At18:45:21.450880 UTC the configured worker logged
execution-readiness-blocked: workflow-resource:capacity-exhausted. PR1954's
preflight deferral finished at1794249921432ms and is the only command
completion in that second. This is a time-correlated worker diagnostic, not
an explicit per-workflow resource receipt. The class is established; the
underlying host/ancestor/aggregate resource inequality remains unknown.

This is distinct from GitHub's Actions outage and from assuming the retained
native-storage8GiB limit is the cause. No arbitrary limit increase, native
release, model call, duplicate request or service override followed. The
command remains durable and its actual admission events remain the spend
source of truth. Precise preflight observation/reporting still needs closure.

## Required Linux jobs restored after provider recovery

GitHub's official summary updated18:43:24 UTC reports Actions operational
and the incident monitoring. Only the terminal failed workflows missing
required Linux jobs were retried on unchanged3debdb8d. Apple Notes attempt3
(exporter129365695903), bridge attempt3 (129365693482), and router attempt2
(all five required Linux jobs, including codex-redirect) now pass. MacOS was
not made a merge requirement.

The failed targeted job was then normally rerun so its required aggregate
can validate those real results: run42673227287 attempt3/job129368273291 is
in progress on the same head. No status was manually marked passing and no
missing check waived. Current-head1758-test code GREEN remains separate
from the aggregate. PR1954 stays ready/queued and its configured-worker
capacity refusal remains a separate unresolved execution hold.

## Passive memory-headroom reference cases on the actual worker

A read-only shared-lock observation at19:05:58 UTC uses current main's
headroom function against the selected autopr service aggregate. Process
birth/membership remained stable. This enters no cgroup, copies no environment,
creates no reservation and performs no reclaim/cleanup/limit change.
[Raw numeric receipt](1703-worker-memory-headroom-observation.json) carries
no private paths or native identifiers.

A4GiB reference request is refused for host_headroom: existing reservation
slack3,867,926,528 bytes plus requested4,294,967,296 plus268,435,456 margin
requires8,431,329,280 bytes (7.85GiB), versus6,377,078,784 available (5.94GiB).
A6GiB reference instead fails aggregate_reservations:4,772,835,328 reserved
plus6,442,450,944 requested exceeds the10,737,418,240-byte service ceiling.

These are deliberately labelled reference vectors, not a captured PR1954
preflight policy or its original refusal. They establish that unused peaks
already promised to active runs matter alongside current usage. They do not
authorize shrinking demand to measured RSS, raising a budget, retiring native
state, declaring whole-machine capacity or closing C0. Precise worker-side
preflight observation, automatic host sizing and full profile/accounting/
publication integration remain open under1917/1918.

## Final published-input CI GREEN and native worker execution

On3debdb8d, targeted run42673227287 attempt3/job129368273291 succeeds,
including1758 CLI tests,0 failed,33 existing ignored and the full required
Linux aggregate. All14 reported CheckRuns pass, with no failed/pending
CheckRun. This is not an independent native review verdict.

Fresh normal status now shows PR1954 at build/acceptance_failed with
repair_implementation next, after seven native model admission events.
The original protected audit records six Codex and one Claude admissions.
The worker moved beyond its initial free preflight hold without changing
limits or resubmitting the command. Its original resource records include
4GiB and6GiB admitted envelopes within the10GiB service aggregate. The earlier
reference refusals cannot be promoted into a permanent policy prohibition.

The active worker candidate fcc76249 is distinct from published input3debdb8d;
input CI does not certify that candidate. Preserve the active branch, body,
original request and protected verifiers. The command-proof schema now labels
current_attempt_row_count explicitly: that mutable per-workflow row is not
a lifetime attempt counter. No review, merge, deployment, automatic capacity
activation, original1944 replay or full1703 acceptance is claimed.
